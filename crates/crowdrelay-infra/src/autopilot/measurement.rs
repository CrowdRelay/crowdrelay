//! Split PostgreSQL Autopilot adapter implementation.

mod fan_windows;
mod observation;
mod readiness;

use super::*;
use readiness::{
    dispatch_reached_an_audience, measured_evidence_quality, refresh_evidence_readiness,
    refresh_experiment_readiness,
};

#[async_trait]
impl AutopilotMeasurementRepository for PostgresAutopilotRepository {
    async fn claim_due_measurements(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedAutopilotMeasurement>, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            // A stale-exhausted measurement is a terminal failure detected
            // outside the worker: like `fail_measurement`'s terminal arm it
            // must close readiness, or the evidence row stays open forever
            // and every metric — harm keys included — its siblings already
            // wrote never reaches the learner. No harm merges here: the
            // crashed attempt's observation never left its memory, so the
            // row closes with whatever earlier attempts actually landed.
            let stale_failed: Vec<(uuid::Uuid, String)> = sqlx::query_as(
                r#"
                UPDATE autopilot_measurements
                SET status = 'failed', finished_at = $2, last_error_kind = 'stale_retry_exhausted'
                WHERE workspace_id = $1
                  AND status = 'processing'
                  AND started_at <= $2 - INTERVAL '15 minutes'
                  AND attempt_count >= 3
                RETURNING action_id, measurement_kind
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .fetch_all(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            for (action_id, kind_str) in stale_failed {
                let measurement_kind = super::parse_measurement_kind(&kind_str)
                    .unwrap_or(super::AutopilotMeasurementKind::AgentRunFanGrowth14d);
                refresh_evidence_readiness(
                    &mut transaction,
                    workspace_id,
                    AutopilotActionId::from(action_id),
                    Some(measurement_kind),
                    now,
                )
                .await?;
            }
            sqlx::query(
                r#"
                UPDATE autopilot_measurements
                SET status = 'pending', available_at = $2, started_at = NULL,
                    last_error_kind = 'stale_processing_recovered'
                WHERE workspace_id = $1
                  AND status = 'processing'
                  AND started_at <= $2 - INTERVAL '15 minutes'
                  AND attempt_count < 3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let with_tasks = sqlx::query_scalar::<_, bool>(
                "SELECT to_regclass('agent_service_tasks') IS NOT NULL",
            )
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let claim_sql = fan_windows::claim_sql(with_tasks);
            let rows = sqlx::query_as::<_, ClaimedMeasurementRow>(&claim_sql)
                .bind(workspace_id.into_uuid())
                .bind(now)
                .bind(i64::from(limit.min(100)))
                .fetch_all(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            // Parse while the claim transaction is still open. A DB/Rust enum
            // drift must never commit a whole batch as `processing` and strand the
            // valid rows behind stale-recovery. Quarantine only the unsupported row.
            let mut claimed = Vec::with_capacity(rows.len());
            for row in rows {
                let measurement_id = row.id;
                match claimed_measurement(row) {
                    Ok(measurement) => claimed.push(measurement),
                    Err(_) => {
                        let quarantined: Option<(uuid::Uuid,)> = sqlx::query_as(
                            r#"
                            UPDATE autopilot_measurements
                            SET status='failed', finished_at=$3,
                                last_error_kind='unsupported_measurement_kind'
                            WHERE workspace_id=$1 AND id=$2 AND status='processing'
                            RETURNING action_id
                            "#,
                        )
                        .bind(workspace_id.into_uuid())
                        .bind(measurement_id)
                        .bind(now)
                        .fetch_optional(&mut *transaction)
                        .await
                        .map_err(map_sqlx)?;
                        if let Some((action_id,)) = quarantined {
                            // The kind never parsed, so no horizon cursor can
                            // be named — `None` still lets the row's full
                            // resolution run, which is the part that cannot
                            // be skipped: a quarantined measurement holding
                            // the queue open strands its evidence row.
                            refresh_evidence_readiness(
                                &mut transaction,
                                workspace_id,
                                AutopilotActionId::from(action_id),
                                None,
                                now,
                            )
                            .await?;
                        }
                    }
                }
            }
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(claimed)
        })
        .await
    }

    async fn observe_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        now: OffsetDateTime,
    ) -> Result<f64, RepositoryError> {
        self.bounded(observation::observe(
            &self.pool,
            workspace_id,
            measurement,
            now,
        ))
        .await
    }

    async fn observe_action_harm(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        _now: OffsetDateTime,
    ) -> Result<HarmObservation, RepositoryError> {
        self.bounded(observation::harm::observe_harm(
            &self.pool,
            workspace_id,
            measurement,
        ))
        .await
    }

    async fn complete_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        observed_value: f64,
        effect: EffectResult,
        harm: Option<&HarmObservation>,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.bounded(async {
            if !observed_value.is_finite() {
                return Err(RepositoryError::Unexpected);
            }
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let metric_key = format!("effect.{}", measurement.kind.as_str());
            let assessment = effect_assessment_str(effect.assessment);
            let outcome_inserted = sqlx::query(
                r#"
                INSERT INTO autopilot_outcomes (
                    workspace_id, decision_id, action_id, measurement_id, metric_key,
                    observed_value, baseline_value, effect_assessment, delta_basis_points,
                    metadata, observed_at
                )
                SELECT $1, action.decision_id, action.id, $3, $4, $5, $6, $7, $8, $9, $10
                FROM autopilot_actions AS action
                WHERE action.workspace_id = $1 AND action.id = $2
                ON CONFLICT (workspace_id, measurement_id)
                    WHERE measurement_id IS NOT NULL DO NOTHING
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(measurement.action_id.into_uuid())
            .bind(measurement.id.into_uuid())
            .bind(metric_key)
            .bind(observed_value)
            // The attributed fan kinds are measured against zero; the rate a
            // measurement scheduled before that change still carries is not
            // what this outcome was compared with, and must not sit beside it.
            .bind(if measurement.kind.counts_attributed_fans() {
                0.0
            } else {
                measurement.baseline_value
            })
            .bind(assessment)
            .bind(effect.delta_basis_points)
            .bind(json!({
                "measurement_kind": measurement.kind.as_str(),
            }))
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if outcome_inserted.rows_affected() == 0 {
                // The action row is missing — the outcome INSERT...SELECT
                // produced no rows. Fail the measurement instead of marking
                // it succeeded with no outcome recorded.
                return Err(RepositoryError::NotFound);
            }
            let updated = sqlx::query(
                r#"
                UPDATE autopilot_measurements
                SET status = 'succeeded', finished_at = $3, last_error_kind = NULL
                WHERE workspace_id = $1 AND id = $2 AND status = 'processing'
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(measurement.id.into_uuid())
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if updated.rows_affected() != 1 {
                return Err(RepositoryError::Conflict);
            }
            // Bridge: resolve the dispatch prediction with the observed
            // outcome. The brain records predictions in
            // dispatch_predictions before dispatch; the measurement
            // system writes outcomes to autopilot_outcomes. Without
            // this bridge, the prediction's observed_new_fans /
            // resolved_at columns are never populated, and the causal model
            // learns from an empty dataset every cycle.
            //
            // We map measurement kinds to prediction columns:
            //   agent_run_fan_growth_14d      → observed_new_fans
            //   incremental_fan_growth_14d    → observed_new_fans (preferred)
            //   agent_run_signal_installs_7d  → observed_signal_installs
            //
            // The evidence view (brain_evidence) also joins these
            // tables, so even if this bridge misses a row, the view
            // provides the join. But updating the prediction row directly
            // is more efficient for the brain's read path.
            match measurement.kind {
                // Engagement is the fastest signal this product can observe --
                // upvotes and comments arrive in hours, in tens, per post.
                // This arm writes the typed `observed_engagement` column the
                // evidence view joins on; the learned copy rides the generic
                // `learnable_metric_key` merge below, which lands the value in
                // `observed_metrics` under `engagement_score` for the metric
                // posteriors to consume.
                AutopilotMeasurementKind::AgentRunCommunityEngagement7d => {
                    let _ = sqlx::query(
                        r#"
                        UPDATE growth_evidence
                        SET observed_engagement = $3
                        WHERE workspace_id = $1
                          AND action_id = $2
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_id.into_uuid())
                    .bind(observed_value)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                }
                AutopilotMeasurementKind::AgentRunFanGrowth14d => {
                    let _ = sqlx::query(
                        r#"
                        UPDATE dispatch_predictions
                        -- Each measurement writes its own column and nothing
                        -- else. `resolved_at` is set by
                        -- `refresh_evidence_readiness` once the queue is empty,
                        -- because readiness is a fact about the whole set of
                        -- outcomes and no single measurement can speak for it.
                        --
                        -- The fourteen-day count is definitive and replaces
                        -- the three-day one. It used to be written only
                        -- `WHERE observed_new_fans IS NULL`, and the three-day
                        -- checkpoint always lands first, so this column held
                        -- three-day counts for every dispatch.
                        SET observed_new_fans = $3
                        WHERE workspace_id = $1
                          AND action_id = $2
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_id.into_uuid())
                    .bind(observed_value)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                    // Also update the growth evidence table. The 14d
                    // observation is the definitive count — it overwrites
                    // the 3d intermediate value unconditionally.
                    let _ = sqlx::query(
                        r#"
                        UPDATE growth_evidence
                        SET observed_fans = $3
                        WHERE workspace_id = $1
                          AND action_id = $2
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_id.into_uuid())
                    .bind(observed_value)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                }
                // Early 3-day checkpoint: writes to BOTH the dispatch
                // prediction and the growth evidence row. The 3-day value
                // is an intermediate observation — it undercounts vs the
                // 14-day count, but it is a real observation the brain can
                // learn from with downweighted evidence quality.
                //
                // The evidence row's observed_fans is set with
                // COALESCE(observed_fans, $3) so the 14d measurement (which
                // overwrites unconditionally in its own arm below) replaces
                // this intermediate value when it arrives.
                //
                // The partial resolution count (incremented in
                // refresh_evidence_readiness) makes the row eligible for
                // delta replay. Without this write, the replay would load
                // the row but skip the outcome model update because
                // observed_fans was NULL — a no-op learning loop.
                AutopilotMeasurementKind::AgentRunFanGrowth3d => {
                    let _ = sqlx::query(
                        r#"
                        UPDATE dispatch_predictions
                        SET observed_new_fans = $3
                        WHERE workspace_id = $1
                          AND action_id = $2
                          AND observed_new_fans IS NULL
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_id.into_uuid())
                    .bind(observed_value)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                    let _ = sqlx::query(
                        r#"
                        UPDATE growth_evidence
                        SET observed_fans = COALESCE(observed_fans, $3)
                        WHERE workspace_id = $1
                          AND action_id = $2
                          AND observed_fans IS NULL
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_id.into_uuid())
                    .bind(observed_value)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                }
                // IncrementalFanGrowth14d is the counterfactual-adjusted
                // value. It is available to the brain via the evidence
                // view's observed_incremental_fans column, so we don't
                // write it to observed_new_fans (which holds the raw
                // count only).
                AutopilotMeasurementKind::IncrementalFanGrowth14d => {
                    let evidence_quality =
                        measured_evidence_quality(&mut transaction, workspace_id, measurement)
                            .await?;
                    let _ = sqlx::query(
                        r#"
                        UPDATE growth_evidence
                        SET observed_incremental_fans = COALESCE(observed_incremental_fans, $3),
                            evidence_quality = $4
                        WHERE workspace_id = $1
                          AND action_id = $2
                          AND observed_incremental_fans IS NULL
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_id.into_uuid())
                    .bind(observed_value)
                    .bind(evidence_quality)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                }
                // Its own column, never `observed_incremental_fans`.
                //
                // The fourteen-day write above is `COALESCE(..., $3) WHERE ...
                // IS NULL`, so the first writer wins — a three-day estimate
                // landing in that column would permanently block the better
                // number that arrives eleven days later. Separate columns let
                // the learner prefer the fourteen-day estimate wherever it
                // exists and fall back to this one only while it does not.
                //
                // `evidence_quality` is deliberately not written here. That
                // column describes the row's best available evidence, and a
                // three-day proxy must not downgrade a row that is going to
                // carry a fourteen-day randomised outcome.
                AutopilotMeasurementKind::IncrementalFanGrowth3d => {
                    let _ = sqlx::query(
                        r#"
                        UPDATE growth_evidence
                        SET observed_incremental_fans_3d =
                                COALESCE(observed_incremental_fans_3d, $3)
                        WHERE workspace_id = $1
                          AND action_id = $2
                          AND observed_incremental_fans_3d IS NULL
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_id.into_uuid())
                    .bind(observed_value)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                }
                AutopilotMeasurementKind::AgentRunSignalInstalls7d
                | AutopilotMeasurementKind::SignalInstalls1d => {
                    // The seven-day count replaces the one-day checkpoint; the
                    // checkpoint only fills an empty column. Both used to be
                    // `WHERE observed_signal_installs IS NULL`, so the
                    // one-day value — which always lands first — stuck.
                    let definitive =
                        measurement.kind == AutopilotMeasurementKind::AgentRunSignalInstalls7d;
                    let _ = sqlx::query(
                        r#"
                        UPDATE dispatch_predictions
                        SET observed_signal_installs = $3
                        WHERE workspace_id = $1
                          AND action_id = $2
                          AND ($4 OR observed_signal_installs IS NULL)
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_id.into_uuid())
                    .bind(observed_value)
                    .bind(definitive)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                    // Nothing else. The signal install measurements have no
                    // column on the evidence row and must not close it either
                    // — Y14 is a week away and Y30 a month past that.
                }
                // DurableFanGrowth30d writes the durable fan count to the
                // growth evidence table's durable_fans_30d column.
                AutopilotMeasurementKind::DurableFanGrowth30d => {
                    let evidence_quality =
                        measured_evidence_quality(&mut transaction, workspace_id, measurement)
                            .await?;
                    let _ = sqlx::query(
                        r#"
                        UPDATE growth_evidence
                        SET durable_fans_30d = COALESCE(durable_fans_30d, $3),
                            evidence_quality = $4
                        WHERE workspace_id = $1
                          AND action_id = $2
                          AND durable_fans_30d IS NULL
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_id.into_uuid())
                    .bind(observed_value)
                    .bind(evidence_quality)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                }
                // Scanner/strategist proximal outcomes have no typed column
                // on the evidence row — the observed value lives in the
                // outcome table, and any kind with a `learnable_metric_key`
                // also lands in `observed_metrics` just below. Readiness
                // closes their evidence once the queue is empty, same as
                // everyone else's.
                _ => {}
            }
            // Every kind with a learnable metric key also lands in the
            // evidence row's `observed_metrics` map — the general write-back
            // that makes the measurement reachable by the metric posteriors.
            // Without it, ticket revenue, replies, clicks and engagement were
            // measured, classified, and then invisible to every learner.
            //
            // The write is additive, not first-wins: a batch action (a gig
            // outreach to four promoters) carries one measurement per
            // recipient under the same key, and a first-wins guard would keep
            // only whichever replied earliest. Each measurement completes
            // exactly once — the status claim is transactional — so adding on
            // completion sums per-subject answers without a re-run ever
            // doubling. The expression reads the row's own map, which Postgres
            // re-fetches under the row lock, so two completions finishing
            // together still both land.
            //
            // The fan-growth kinds return `None` here on purpose: they write
            // typed columns that dedicated posteriors consume, and a value in
            // two places is a value learned twice.
            if let Some(metric_key) = measurement.kind.learnable_metric_key() {
                let _ = sqlx::query(
                    r#"
                    UPDATE growth_evidence
                    SET observed_metrics = observed_metrics ||
                        jsonb_build_object(
                            $3::text,
                            to_jsonb(
                                COALESCE((observed_metrics->>$3)::double precision, 0)
                                + $4::double precision
                            )
                        )
                    WHERE workspace_id = $1
                      AND action_id = $2
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.action_id.into_uuid())
                .bind(metric_key)
                .bind(observed_value)
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            }
            // Harm keys — every observed source, zeros included: a clean
            // reading is the evidence that teaches the posterior the rate
            // is low. Unlike the additive metric merge above, the `||`
            // overwrite is right here: harm counts are action-level totals
            // for a window, not per-subject answers — sibling measurements
            // of one action each observe the same whole, so the
            // latest-closing window wins and nothing can double. `None`
            // means the collector failed — nothing is written rather than
            // zeros a broken observation did not earn.
            if let Some(harm) = harm {
                merge_harm_keys(&mut *transaction, workspace_id, measurement, harm).await?;
            }
            // The autonomy guardrail reacts to evidence about actions only:
            // a workspace-window count is not one, so neither the reading in
            // hand nor earlier ones of those kinds can demote a policy.
            if effect.assessment == EffectAssessment::Worsened
                && !measurement.kind.is_workspace_window()
            {
                let demoted_context = sqlx::query_scalar::<_, String>(
                    r#"
                    WITH action_context AS (
                        SELECT context
                        FROM autopilot_actions
                        WHERE workspace_id=$1 AND id=$2
                    ), latest_per_action AS (
                        SELECT DISTINCT ON (outcome.action_id)
                               outcome.action_id, outcome.effect_assessment,
                               outcome.observed_at, outcome.id
                        FROM autopilot_outcomes outcome
                        JOIN autopilot_actions action
                          ON action.workspace_id=outcome.workspace_id AND action.id=outcome.action_id
                        JOIN action_context ON action.context=action_context.context
                        JOIN autopilot_measurements measured
                          ON measured.workspace_id=outcome.workspace_id
                         AND measured.id=outcome.measurement_id
                        WHERE outcome.workspace_id=$1 AND outcome.measurement_id IS NOT NULL
                          AND measured.measurement_kind <> ALL($4::text[])
                        ORDER BY outcome.action_id, outcome.observed_at DESC, outcome.id DESC
                    ), recent AS (
                        SELECT effect_assessment
                        FROM latest_per_action
                        ORDER BY observed_at DESC, id DESC
                        LIMIT 2
                    ), qualifies AS (
                        SELECT count(*)=2 AND bool_and(effect_assessment='worsened') AS should_guard
                        FROM recent
                    )
                    UPDATE autopilot_policies policy
                    SET autonomy_level='require_approval',
                        guarded_until=$3 + INTERVAL '7 days',
                        guardrail_reason='two_consecutive_worsened_effects',
                        version=version+1
                    FROM action_context, qualifies
                    WHERE policy.workspace_id=$1
                      AND policy.context=action_context.context
                      AND policy.enabled
                      AND policy.autonomy_level='bounded_auto'
                      AND qualifies.should_guard
                    RETURNING policy.context
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.action_id.into_uuid())
                .bind(now)
                .bind(AutopilotMeasurementKind::WORKSPACE_WINDOW_KINDS.to_vec())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                if let Some(context) = demoted_context {
                    sqlx::query(
                        r#"
                        INSERT INTO outbox_events (workspace_id,event_type,event_version,payload,max_attempts)
                        VALUES ($1,'crowdrelay.autopilot.authority_demoted',1,
                            jsonb_build_object(
                                'context',$2::text,
                                'reason','two_consecutive_worsened_effects',
                                'guarded_until',$3::timestamptz + INTERVAL '7 days'
                            ),12)
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(context)
                    .bind(now)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                }
            }
            // Enqueue attribution request for fan-growth measurements.
            // The attribution worker discovers competing actions, runs
            // the CreditAllocator, and writes credited entries to the
            // credit ledger. This is durable — if the transaction
            // commits, the attribution will eventually happen.
            if matches!(
                measurement.kind,
                AutopilotMeasurementKind::AgentRunFanGrowth14d
                    | AutopilotMeasurementKind::IncrementalFanGrowth14d
                    | AutopilotMeasurementKind::DurableFanGrowth30d
            ) {
                let _ = sqlx::query(
                    r#"
                    INSERT INTO attribution_requests
                        (workspace_id, measurement_id, action_id, attribution_version)
                    VALUES ($1, $2, $3, 1)
                    ON CONFLICT (measurement_id, attribution_version) DO NOTHING
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.id.into_uuid())
                .bind(measurement.action_id.into_uuid())
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            }
            // Look up the experiment assignment for this action to
            // evaluate contamination over the full measurement window.
            // CONTAMINATION IS EVALUATED OVER THE FULL WINDOW — not just
            // assignment time. A clean assignment can become contaminated
            // later if concurrent treatment actions occur on the same unit.
            let experiment_info: Option<(uuid::Uuid, String, time::OffsetDateTime)> =
                if matches!(
                    measurement.kind,
                    AutopilotMeasurementKind::AgentRunFanGrowth14d
                        | AutopilotMeasurementKind::IncrementalFanGrowth14d
                        | AutopilotMeasurementKind::DurableFanGrowth30d
                ) {
                    sqlx::query_as::<_, (sqlx::types::Uuid, String, time::OffsetDateTime)>(
                        r#"
                        SELECT experiment_uuid, unit_id, assigned_at
                        FROM experiment_assignments
                        WHERE workspace_id = $1
                          AND action_id = $2
                          AND experiment_uuid IS NOT NULL
                        LIMIT 1
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_id.into_uuid())
                    .fetch_optional(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?
                } else {
                    None
                };
            // Contamination is established inside this transaction, with the
            // outcome it qualifies. It used to run after the commit with its
            // result dropped, so a failure left the assignment's
            // `final_contamination` NULL while the evidence row went on saying
            // `randomized_holdout` — an unbacked claim of clean causal
            // evidence that nothing downstream could detect. Committed
            // together, the outcome and what is known about its cleanliness
            // cannot disagree; and a failure here rolls the outcome back so
            // the measurement is retried rather than half-recorded.
            if let Some((exp_uuid, unit_id, assigned_at)) = experiment_info {
                super::operations::experiment_assignments::evaluate_contamination(
                    &mut transaction,
                    workspace_id,
                    exp_uuid,
                    &unit_id,
                    assigned_at,
                    now,
                )
                .await?;
                // The control arm is measured in the same breath. A control
                // unit is never dispatched, so nothing schedules a measurement
                // for it, so its evidence would sit unresolved forever while
                // the treatment rows resolved around it — leaving the learner
                // with treatment-only data under an intent-to-treat label.
                super::operations::experiment_assignments::resolve_control_evidence(
                    &mut transaction,
                    workspace_id,
                    exp_uuid,
                    now,
                )
                .await?;
                // Readiness for the whole experiment, not just this action. The
                // control arm may have resolved a moment ago in this very
                // transaction, releasing treated rows that finished their own
                // measurements days earlier and have been waiting for it.
                refresh_experiment_readiness(&mut transaction, workspace_id, exp_uuid, now).await?;
            }
            // The queue decides readiness, and it decides it after this
            // measurement has been marked terminal above, so an action whose
            // last outcome just landed closes here and one still waiting on a
            // fourteen- or forty-four-day window does not. Runs after the
            // control sweep so a treated row released by it is not held for
            // another cycle.
            refresh_evidence_readiness(&mut transaction, workspace_id, measurement.action_id, Some(measurement.kind), now)
                .await?;
            transaction.commit().await.map_err(map_sqlx)?;
            // Close any experiment designs whose measurement windows have
            // elapsed and whose evidence is fully resolved. This is the
            // lifecycle signal that a design is done — without it, designs
            // accumulate as `active` indefinitely.
            super::operations::experiment_assignments::close_completed_experiments(
                &self.pool,
                workspace_id,
                now,
            )
            .await?;
            Ok(())
        })
        .await
    }

    async fn fail_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        error_kind: &'static str,
        retryable: bool,
        harm: Option<&HarmObservation>,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let row: Option<(uuid::Uuid, String, String)> =
                sqlx::query_as::<_, (uuid::Uuid, String, String)>(
                    r#"
                UPDATE autopilot_measurements
                SET status = CASE WHEN $4 AND attempt_count < 3 THEN 'pending' ELSE 'failed' END,
                    available_at = CASE
                        WHEN $4 AND attempt_count < 3 THEN $3 + INTERVAL '30 minutes'
                        ELSE available_at
                    END,
                    started_at = CASE WHEN $4 AND attempt_count < 3 THEN NULL ELSE started_at END,
                    finished_at = CASE WHEN $4 AND attempt_count < 3 THEN NULL ELSE $3 END,
                    last_error_kind = $5
                WHERE workspace_id = $1 AND id = $2 AND status = 'processing'
                RETURNING action_id, measurement_kind, status
                "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(measurement.id.into_uuid())
                .bind(now)
                .bind(retryable)
                .bind(error_kind)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
            // An outcome that will never arrive must not hold the evidence
            // open forever. Readiness re-checks the queue: if this was the
            // last thing outstanding, the row closes with that outcome's
            // column still NULL, and the learner skips what it never learned.
            if let Some((action_id, kind_str, status)) = row {
                // A terminal failure still leaves the harm it observed
                // behind — the harm window elapsed even when the primary
                // metric never read. Merging here, before readiness, means
                // the row closes with the keys already on it: no replay
                // delta can slip between the writes and strand them.
                if status == "failed"
                    && let Some(harm) = harm
                {
                    merge_harm_keys(&mut *transaction, workspace_id, measurement, harm).await?;
                }
                let measurement_kind = super::parse_measurement_kind(&kind_str)
                    .unwrap_or(super::AutopilotMeasurementKind::AgentRunFanGrowth14d);
                refresh_evidence_readiness(
                    &mut transaction,
                    workspace_id,
                    AutopilotActionId::from(action_id),
                    Some(measurement_kind),
                    now,
                )
                .await?;
            }
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(())
        })
        .await
    }
}

/// The `harm:*` keys as a jsonb object — all five, zeros included. A clean
/// send is evidence too: a posterior that only ever saw harmful actions
/// would overstate the rate.
fn harm_keys_value(harm: &HarmObservation) -> Value {
    Value::Object(
        harm.entries()
            .into_iter()
            .map(|(key, value)| (key.to_owned(), json!(value)))
            .collect(),
    )
}

/// Merges the `harm:*` keys onto the evidence row inside the completion
/// transaction — last-writer-wins on each key, deliberately unlike the
/// additive metric merge: a harm count is the action-level total for the
/// window, already summed across fans, so sibling measurements overwriting
/// one another converge on the latest window's whole instead of doubling.
async fn merge_harm_keys<'e, E>(
    executor: E,
    workspace_id: WorkspaceId,
    measurement: &ClaimedAutopilotMeasurement,
    harm: &HarmObservation,
) -> Result<(), RepositoryError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(
        r#"
        UPDATE growth_evidence
        SET observed_metrics = observed_metrics || $3::jsonb
        WHERE workspace_id = $1 AND action_id = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(measurement.action_id.into_uuid())
    .bind(harm_keys_value(harm))
    .execute(executor)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}
