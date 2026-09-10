//! Split PostgreSQL Autopilot adapter implementation.

mod observation;
mod readiness;

use super::*;
use readiness::{
    dispatch_reached_an_audience, measured_evidence_quality, observable_community,
    refresh_evidence_readiness, refresh_experiment_readiness,
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
            sqlx::query(
                r#"
                UPDATE viryaos_autopilot_measurements
                SET status = 'failed', finished_at = $2, last_error_kind = 'stale_retry_exhausted'
                WHERE workspace_id = $1
                  AND status = 'processing'
                  AND started_at <= $2 - INTERVAL '15 minutes'
                  AND attempt_count >= 3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            sqlx::query(
                r#"
                UPDATE viryaos_autopilot_measurements
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
            let rows = sqlx::query_as::<_, ClaimedMeasurementRow>(
                r#"
                WITH selected AS (
                    SELECT id
                    FROM viryaos_autopilot_measurements
                    WHERE workspace_id = $1
                      AND status = 'pending'
                      AND due_at <= $2
                      AND available_at <= $2
                      AND attempt_count < 3
                    ORDER BY due_at, id
                    FOR UPDATE SKIP LOCKED
                    LIMIT $3
                )
                UPDATE viryaos_autopilot_measurements AS measurement
                SET status = 'processing',
                    attempt_count = measurement.attempt_count + 1,
                    started_at = $2,
                    finished_at = NULL,
                    last_error_kind = NULL
                FROM selected
                WHERE measurement.id = selected.id
                RETURNING measurement.id, measurement.action_id, measurement.measurement_kind,
                          measurement.subject_id, measurement.baseline_value,
                          measurement.action_finished_at,
                          measurement.attempt_count AS attempt_number
                "#,
            )
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
                        sqlx::query(
                            r#"
                            UPDATE viryaos_autopilot_measurements
                            SET status='failed', finished_at=$3,
                                last_error_kind='unsupported_measurement_kind'
                            WHERE workspace_id=$1 AND id=$2 AND status='processing'
                            "#,
                        )
                        .bind(workspace_id.into_uuid())
                        .bind(measurement_id)
                        .bind(now)
                        .execute(&mut *transaction)
                        .await
                        .map_err(map_sqlx)?;
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

    async fn complete_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        observed_value: f64,
        effect: EffectResult,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.bounded(async {
            if !observed_value.is_finite() {
                return Err(RepositoryError::Unexpected);
            }
            // Resolved before the transaction opens, from rows another writer
            // committed. `Some` here means the observation above came from the
            // community ledger rather than the workspace fallback, which is
            // what the evidence quality has to reflect.
            //
            // A read failure is answered the same way as "not a community":
            // the row falls back to the workspace comparison and earns the
            // weaker evidence quality that goes with it. That is the safe
            // direction — it under-claims rather than over-claims — and it is
            // better than failing the whole measurement completion over a
            // transient read. But it is a downgrade taken on no evidence, so
            // it is logged rather than swallowed; a run of these is a
            // measurement window silently recording weaker evidence than it
            // observed.
            let community =
                match observable_community(&self.pool, workspace_id, measurement.action_id).await {
                    Ok(community) => community,
                    Err(error) => {
                        tracing::warn!(
                            error = %error,
                            action_id = %measurement.action_id.into_uuid(),
                            workspace_id = %workspace_id.into_uuid(),
                            "could not establish whether this measurement's unit is an \
                             observable community; recording it against the workspace \
                             fallback, which earns weaker evidence quality"
                        );
                        None
                    }
                };
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let metric_key = format!("effect.{}", measurement.kind.as_str());
            let assessment = effect_assessment_str(effect.assessment);
            let outcome_inserted = sqlx::query(
                r#"
                INSERT INTO viryaos_autopilot_outcomes (
                    workspace_id, decision_id, action_id, measurement_id, metric_key,
                    observed_value, baseline_value, effect_assessment, delta_basis_points,
                    metadata, observed_at
                )
                SELECT $1, action.decision_id, action.id, $3, $4, $5, $6, $7, $8, $9, $10
                FROM viryaos_autopilot_actions AS action
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
            .bind(measurement.baseline_value)
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
                UPDATE viryaos_autopilot_measurements
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
            // viryaos_dispatch_predictions before dispatch; the measurement
            // system writes outcomes to viryaos_autopilot_outcomes. Without
            // this bridge, the prediction's observed_new_fans /
            // resolved_at columns are never populated, and the causal model
            // learns from an empty dataset every cycle.
            //
            // We map measurement kinds to prediction columns:
            //   agent_run_fan_growth_14d      → observed_new_fans
            //   incremental_fan_growth_14d    → observed_new_fans (preferred)
            //   agent_run_signal_installs_7d  → observed_signal_installs
            //
            // The evidence view (viryaos_brain_evidence) also joins these
            // tables, so even if this bridge misses a row, the view
            // provides the join. But updating the prediction row directly
            // is more efficient for the brain's read path.
            match measurement.kind {
                AutopilotMeasurementKind::AgentRunFanGrowth14d => {
                    let _ = sqlx::query(
                        r#"
                        UPDATE viryaos_dispatch_predictions
                        -- Each measurement writes its own column and nothing
                        -- else. `resolved_at` is set by
                        -- `refresh_evidence_readiness` once the queue is empty,
                        -- because readiness is a fact about the whole set of
                        -- outcomes and no single measurement can speak for it.
                        SET observed_new_fans = COALESCE(observed_new_fans, $3)
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
                    // Also update the growth evidence table. The 14d
                    // observation is the definitive count — it overwrites
                    // the 3d intermediate value unconditionally.
                    let _ = sqlx::query(
                        r#"
                        UPDATE viryaos_growth_evidence
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
                        UPDATE viryaos_dispatch_predictions
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
                        UPDATE viryaos_growth_evidence
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
                    let evidence_quality = measured_evidence_quality(
                        &mut transaction,
                        workspace_id,
                        measurement,
                        community.as_deref(),
                    )
                    .await?;
                    let _ = sqlx::query(
                        r#"
                        UPDATE viryaos_growth_evidence
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
                        UPDATE viryaos_growth_evidence
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
                    let _ = sqlx::query(
                        r#"
                        UPDATE viryaos_dispatch_predictions
                        SET observed_signal_installs = COALESCE(observed_signal_installs, $3)
                        WHERE workspace_id = $1
                          AND action_id = $2
                          AND observed_signal_installs IS NULL
                        "#,
                    )
                    .bind(workspace_id.into_uuid())
                    .bind(measurement.action_id.into_uuid())
                    .bind(observed_value)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx)?;
                    // Nothing else. The signal install measurements have no
                    // column on the evidence row and must not close it either
                    // — Y14 is a week away and Y30 a month past that. The 1d
                    // checkpoint writes the same column with COALESCE so the
                    // 7d value replaces it when the longer window closes.
                }
                // DurableFanGrowth30d writes the durable fan count to the
                // growth evidence table's durable_fans_30d column.
                AutopilotMeasurementKind::DurableFanGrowth30d => {
                    let evidence_quality = measured_evidence_quality(
                        &mut transaction,
                        workspace_id,
                        measurement,
                        community.as_deref(),
                    )
                    .await?;
                    let _ = sqlx::query(
                        r#"
                        UPDATE viryaos_growth_evidence
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
                // Scanner/strategist proximal outcomes have no column on the
                // evidence row — the observed value lives in the outcome table
                // and these workers acquire no fans. Readiness below closes
                // their evidence once their queue is empty, same as everyone
                // else's.
                _ => {}
            }
            if effect.assessment == EffectAssessment::Worsened {
                let demoted_context = sqlx::query_scalar::<_, String>(
                    r#"
                    WITH action_context AS (
                        SELECT context
                        FROM viryaos_autopilot_actions
                        WHERE workspace_id=$1 AND id=$2
                    ), latest_per_action AS (
                        SELECT DISTINCT ON (outcome.action_id)
                               outcome.action_id, outcome.effect_assessment,
                               outcome.observed_at, outcome.id
                        FROM viryaos_autopilot_outcomes outcome
                        JOIN viryaos_autopilot_actions action
                          ON action.workspace_id=outcome.workspace_id AND action.id=outcome.action_id
                        JOIN action_context ON action.context=action_context.context
                        WHERE outcome.workspace_id=$1 AND outcome.measurement_id IS NOT NULL
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
                    UPDATE viryaos_autopilot_policies policy
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
                    INSERT INTO viryaos_attribution_requests
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
                        FROM viryaos_experiment_assignments
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
        measurement_id: AutopilotMeasurementId,
        error_kind: &'static str,
        retryable: bool,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let row: Option<(uuid::Uuid, String)> = sqlx::query_as::<_, (uuid::Uuid, String)>(
                r#"
                UPDATE viryaos_autopilot_measurements
                SET status = CASE WHEN $4 AND attempt_count < 3 THEN 'pending' ELSE 'failed' END,
                    available_at = CASE
                        WHEN $4 AND attempt_count < 3 THEN $3 + INTERVAL '30 minutes'
                        ELSE available_at
                    END,
                    started_at = CASE WHEN $4 AND attempt_count < 3 THEN NULL ELSE started_at END,
                    finished_at = CASE WHEN $4 AND attempt_count < 3 THEN NULL ELSE $3 END,
                    last_error_kind = $5
                WHERE workspace_id = $1 AND id = $2 AND status = 'processing'
                RETURNING action_id, measurement_kind
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(measurement_id.into_uuid())
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
            if let Some((action_id, kind_str)) = row {
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
