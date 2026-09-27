//! What "resolved" means for a measurement, and who is allowed to say it.
//!
//! Split out of the adapter so the trait implementation stays inside the
//! source-size ratchet. Every function here answers one question the outcome
//! writes depend on: is this community's outcome observable at all, has this
//! action's evidence got everything its model update needs, and did the
//! randomisation survive the window it was measured over.

use super::super::*;

/// Whether an outbound dispatch ever reached an audience.
///
/// Returns `Ok(true)` when at least one of the action's post artifacts is
/// published, and `Ok(false)` when the action produced artifacts and every one
/// of them is still a draft.
///
/// An action with no artifacts at all depends on whether it was supposed to
/// have one. Scanner and strategist dispatches never produce a post and their
/// outcomes are real, so they report `true` — "there was nothing to publish"
/// must not be confused with "it was never published". A publishing kind with
/// no artifact reports `false`: it reached nobody, and the only honest reading
/// of an absent post is that there is no outcome to measure.
///
/// # Why this exists
///
/// Every outbound channel drafts and waits for an operator: Reddit is
/// read-only by policy, and Telegram, Discord and social default to manual.
/// The dispatch is still recorded as a succeeded action, so its measurement
/// comes due on schedule, observes the fans that a post nobody published did
/// not attract, and records a real zero.
///
/// That zero is not an outcome. It is the absence of one, and the brain cannot
/// tell the difference: the strategy posterior learns that the template does
/// not work, the hypothesis lifecycle degrades it toward Retired, and the
/// dispatch budget moves away from the one channel that would have worked if
/// anyone had pressed publish. The system teaches itself that its own backlog
/// is evidence.
///
/// `observable_community` already refuses this for one measurement kind on one
/// channel. This is the same rule for all four channels and every kind.
pub(in crate::autopilot) async fn dispatch_reached_an_audience(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
) -> Result<bool, RepositoryError> {
    // One query over the four post tables. `bool_or` is NULL when the action
    // produced no artifact at all, and the COALESCE decides what that means.
    //
    // 'posted' is the published state in all four vocabularies; every other
    // state ('pending', 'posting', 'failed', 'rate_limited',
    // 'awaiting_manual_post') means no audience saw it. Matching on the
    // published state rather than excluding the draft one keeps a future
    // status from silently counting as published.
    //
    // The COALESCE default is per action kind, not a constant.
    //
    // "No artifact" means two opposite things. A scanner or the strategist
    // never produces one, and its outcome is real — defaulting those to `true`
    // is what lets them be measured at all. A publishing action that produced
    // none did not reach anybody, and defaulting it to `true` measures the
    // fans a post that does not exist did not attract, then teaches the
    // template that it does not work.
    //
    // That gap is reachable. The three executors claim
    // `agent.content.request` by the agent task's `template_id`, and the
    // social one additionally requires
    // `platform IN ('instagram','facebook','x')` — while the agents service's
    // own schema lets a `social-post` draft carry `telegram` or `discord`.
    // Such a draft is claimed by nobody: social skips it on platform, telegram
    // and discord skip it on template. The action stays `succeeded` with no
    // artifact for good, and before this it was then measured as a real zero.
    //
    // Publishing kinds are listed explicitly rather than inferred, so a new
    // kind is opted in deliberately and an unrecognised one keeps the old
    // permissive default instead of silently becoming unmeasurable.
    let reached = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT COALESCE(
                   bool_or(published),
                   NOT EXISTS (
                       SELECT 1 FROM autopilot_actions a
                       WHERE a.workspace_id = $1 AND a.id = $2
                         AND a.action_kind IN ('agent.content.request',
                                               'community.engage.request')
                   )
               )
        FROM (
            SELECT status = 'posted' AS published
            FROM community_posts
            WHERE workspace_id = $1 AND action_id = $2
            UNION ALL
            SELECT status = 'posted' AS published
            FROM telegram_posts
            WHERE workspace_id = $1 AND action_id = $2
            UNION ALL
            SELECT status = 'posted' AS published
            FROM discord_posts
            WHERE workspace_id = $1 AND action_id = $2
            UNION ALL
            SELECT status = 'posted' AS published
            FROM social_posts
            WHERE workspace_id = $1 AND action_id = $2
        ) AS artifacts
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(reached)
}

/// Marks an action's evidence complete once every measurement it is waiting on
/// has reached a terminal state.
///
/// `resolved_at` answers "is this row ready for the model", and only the
/// measurement queue knows that. Each measurement used to stamp the column
/// itself, which made the earliest arrival — signal installs at seven days —
/// speak for outcomes that were still fourteen and forty-four days away. The
/// row then looked finished while `observed_incremental_fans` and
/// `durable_fans_30d` were still empty, and, because the delta cursor moves
/// with it, it looked finished at the one moment it had the least to teach.
///
/// A failed measurement counts as terminal. An outcome that will never arrive
/// must not hold the evidence open forever; the column stays NULL and the
/// learner skips it, which is the honest reading of "we tried and could not
/// find out".
///
/// When `measurement_kind` is `Some`, stamps the per-horizon replay cursor
/// for the horizon that just completed. When `None`, skips per-horizon
/// stamping and only checks for full resolution — used by the experiment
/// readiness sweep, which re-checks treated rows after the control arm
/// resolves but must not stamp cursors for horizons whose measurements
/// haven't completed yet.
pub(super) async fn refresh_evidence_readiness(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    measurement_kind: Option<super::AutopilotMeasurementKind>,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    // ── Per-horizon replay cursor ──
    //
    // Each measurement horizon (3d, 14d, 30d) is an independent observation.
    // We stamp the per-horizon replay cursor for the horizon that just
    // completed, so the delta loader can pick up this row and the replay
    // logic can gate which posteriors to update. This replaces the single
    // partial_resolution_count increment, which double-counted: the 30d
    // measurement incremented it without changing observed_fans, so the
    // 14d observation was replayed again as a no-op for the outcome model
    // but a full update for the Y14 treatment-effect posterior.
    //
    // We still increment partial_resolution_count and stamp
    // last_partial_resolution_at for backward compatibility and display —
    // they are no longer the delta cursor.
    //
    // When `measurement_kind` is `None` (experiment readiness sweep), we
    // skip per-horizon stamping entirely — the treated rows' own
    // measurements already stamped their cursors, and rows whose
    // measurements haven't completed yet must not get a cursor stamped
    // by a control-arm resolution.
    if let Some(kind) = measurement_kind {
        let horizon_column = match kind {
            // Both three-day kinds stamp the same cursor: they observe the
            // same window and the cursor is a statement about which horizon
            // the learner has already seen, not about which query produced it.
            super::AutopilotMeasurementKind::AgentRunFanGrowth3d
            | super::AutopilotMeasurementKind::IncrementalFanGrowth3d => "replayed_3d_at",
            super::AutopilotMeasurementKind::AgentRunFanGrowth14d
            | super::AutopilotMeasurementKind::IncrementalFanGrowth14d => "replayed_14d_at",
            super::AutopilotMeasurementKind::DurableFanGrowth30d => "replayed_30d_at",
            // Signal installs and other measurements don't have a
            // per-horizon cursor — they update the legacy
            // partial_resolution_count only. The delta loader still
            // picks them up via last_partial_resolution_at as a fallback.
            _ => "last_partial_resolution_at",
        };
        // When the horizon column IS last_partial_resolution_at, setting it
        // twice in the same SET clause is a PostgreSQL error ("column
        // specified more than once"). The explicit last_partial_resolution_at
        // line is only needed for per-horizon columns, to also advance the
        // legacy cursor alongside the per-horizon one.
        let legacy_cursor = if horizon_column == "last_partial_resolution_at" {
            ""
        } else {
            ", last_partial_resolution_at = $3"
        };
        let partial_result = sqlx::query(&format!(
            r#"
            UPDATE growth_evidence AS evidence
            SET {horizon_column} = $3,
                partial_resolution_count = evidence.partial_resolution_count + 1{legacy_cursor}
            WHERE evidence.workspace_id = $1
              AND evidence.action_id = $2
              AND evidence.resolved_at IS NULL
              AND {horizon_column} IS NULL
            "#,
        ))
        .bind(workspace_id.into_uuid())
        .bind(action_id.into_uuid())
        .bind(now)
        .execute(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
        if partial_result.rows_affected() > 0 {
            tracing::info!(
                workspace_id = %workspace_id.into_uuid(),
                action_id = %action_id.into_uuid(),
                rows = partial_result.rows_affected(),
                horizon = horizon_column,
                "evidence readiness: per-horizon replay cursor stamped — intermediate checkpoint available for learning"
            );
        }
    }

    // ── Full resolution ──
    //
    // Only when every measurement has reached a terminal state AND the
    // control arm has resolved (or its 44-day window has elapsed) do we
    // stamp resolved_at. The delta cursor moves with resolved_at, so
    // premature full resolution would skip the long-horizon outcome
    // entirely.
    let evidence_result = sqlx::query(
        r#"
        UPDATE growth_evidence AS evidence
        SET resolved_at = $3
        WHERE evidence.workspace_id = $1
          AND evidence.action_id = $2
          AND evidence.resolved_at IS NULL
          AND NOT EXISTS (
              SELECT 1
              FROM autopilot_measurements AS outstanding
              WHERE outstanding.workspace_id = evidence.workspace_id
                AND outstanding.action_id = evidence.action_id
                AND outstanding.status IN ('pending', 'processing')
          )
          -- The control arm's outcome is one of the outcomes this row's model
          -- update requires. Under intent-to-treat the treated unit is compared
          -- against the units the action was withheld from, so a treated row
          -- whose control arm has not been measured yet is not model-ready —
          -- it would be replayed alone, contrasted against nothing, and
          -- consumed. The delta cursor moves past it and it is never seen
          -- again, so "wait" is the only correct answer here.
          --
          -- Bounded by the control's own measurement window: once that has
          -- elapsed the control resolves (in this same transaction, just
          -- above), so this clause clears itself rather than holding evidence
          -- open on an outcome that will never arrive.
          AND NOT EXISTS (
              SELECT 1
              FROM experiment_assignments AS treated
              JOIN experiment_assignments AS control
                ON control.workspace_id = treated.workspace_id
               AND control.experiment_uuid = treated.experiment_uuid
               AND control.arm = 'control'
              JOIN growth_evidence AS control_evidence
                ON control_evidence.workspace_id = control.workspace_id
               AND control_evidence.experiment_assignment_id = control.id
              WHERE treated.workspace_id = evidence.workspace_id
                AND treated.action_id = evidence.action_id
                AND treated.arm = 'treatment'
                AND control_evidence.resolved_at IS NULL
                AND control.assigned_at + INTERVAL '44 days' > $3
          )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    let evidence_resolved = evidence_result.rows_affected();
    if evidence_resolved > 0 {
        tracing::info!(
            workspace_id = %workspace_id.into_uuid(),
            action_id = %action_id.into_uuid(),
            rows = evidence_resolved,
            "evidence readiness: fully resolved evidence row(s)"
        );
    }
    let prediction_result = sqlx::query(
        r#"
        UPDATE dispatch_predictions AS prediction
        SET resolved_at = $3
        WHERE prediction.workspace_id = $1
          AND prediction.action_id = $2
          AND prediction.resolved_at IS NULL
          AND NOT EXISTS (
              SELECT 1
              FROM autopilot_measurements AS outstanding
              WHERE outstanding.workspace_id = prediction.workspace_id
                AND outstanding.action_id = prediction.action_id
                AND outstanding.status IN ('pending', 'processing')
          )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if prediction_result.rows_affected() > 0 {
        tracing::info!(
            workspace_id = %workspace_id.into_uuid(),
            action_id = %action_id.into_uuid(),
            rows = prediction_result.rows_affected(),
            "evidence readiness: resolved dispatch prediction row(s)"
        );
    }
    Ok(())
}

/// The evidence quality this measurement actually earned.
///
/// The fan kinds now read fans traced to the action itself
/// (`counts_attributed_fans`), so the outcome is always read at the level of
/// the unit the action treated. A randomised assignment therefore earns
/// `randomized_holdout`; everything else is `observational` — a traced count
/// with a structurally-zero counterfactual, weighted like any observational
/// row (decided 2026-09-27, `ATTRIBUTED_OUTCOME_PLAN.md` §8). It used to be
/// `matched_quasi_experiment` whenever the workspace fallback answered,
/// which described a pre/post subtraction that no longer happens.
pub(super) async fn measured_evidence_quality(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    measurement: &ClaimedAutopilotMeasurement,
) -> Result<&'static str, RepositoryError> {
    let experiment_kind: Option<String> = sqlx::query_scalar::<_, String>(
        r#"
        SELECT experiment_kind
        FROM experiment_assignments
        WHERE workspace_id = $1
          AND action_id = $2
          AND experiment_uuid IS NOT NULL
        LIMIT 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(measurement.action_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    Ok(
        if experiment_kind.as_deref() == Some("randomized_holdout") {
            "randomized_holdout"
        } else {
            "observational"
        },
    )
}

/// Re-checks readiness for every treated row of an experiment once its control
/// arm has been measured.
///
/// The per-action check only ever looks at the action whose measurement just
/// completed. Treated rows held back waiting for the control would otherwise
/// stay held forever: the control resolves in one action's transaction, and
/// nothing revisits the eight actions that finished earlier. Sweeping the
/// experiment closes all of them in the same transaction as the control, which
/// is also what puts them in the same delta batch — the contrast is computed
/// per batch, so arriving together is the difference between an intent-to-treat
/// estimate and a pre/post one.
pub(super) async fn refresh_experiment_readiness(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    experiment_uuid: uuid::Uuid,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let treated: Vec<uuid::Uuid> = sqlx::query_scalar::<_, uuid::Uuid>(
        r#"
        SELECT assignment.action_id
        FROM experiment_assignments AS assignment
        JOIN growth_evidence AS evidence
          ON evidence.workspace_id = assignment.workspace_id
         AND evidence.action_id = assignment.action_id
        WHERE assignment.workspace_id = $1
          AND assignment.experiment_uuid = $2
          AND assignment.arm = 'treatment'
          AND assignment.action_id IS NOT NULL
          AND evidence.resolved_at IS NULL
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(experiment_uuid)
    .fetch_all(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    for action_id in treated {
        refresh_evidence_readiness(
            transaction,
            workspace_id,
            AutopilotActionId::from(action_id),
            None,
            now,
        )
        .await?;
    }
    Ok(())
}
