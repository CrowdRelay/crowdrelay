//! What "resolved" means for a measurement, and who is allowed to say it.
//!
//! Split out of the adapter so the trait implementation stays inside the
//! source-size ratchet. Every function here answers one question the outcome
//! writes depend on: is this community's outcome observable at all, has this
//! action's evidence got everything its model update needs, and did the
//! randomisation survive the window it was measured over.

use super::super::*;

/// Resolves the community an action's experiment unit refers to, but only
/// once that community has actually been posted to.
///
/// Returns `Ok(None)` when the unit is not a community, which is the caller's
/// signal to fall back to the workspace-level comparison.
///
/// Two things went wrong here before and both are guarded by the same
/// function. The unit id on a community assignment is an
/// `agent_outreach_targets` UUID, while `fan_provenance_events.community`
/// holds the handle the smart link was tagged with — `r/metalmemes`. Querying
/// the ledger with the UUID matched nothing, and `COUNT` answers "nothing"
/// with a zero rather than a NULL, so the miss arrived looking exactly like a
/// community that had genuinely converted no one. The handle lookup fixes the
/// key; requiring a published post fixes the rest, because a community whose
/// post is still a draft has no outcome to report and must say so instead of
/// reporting zero.
pub(super) async fn observable_community(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
) -> Result<Option<String>, RepositoryError> {
    let unit: Option<(String, String)> = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT unit_id, unit_kind
        FROM viryaos_experiment_assignments
        WHERE workspace_id = $1
          AND action_id = $2
          AND experiment_uuid IS NOT NULL
        LIMIT 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx)?;
    let Some((unit_id, unit_kind)) = unit else {
        return Ok(None);
    };
    if unit_kind != "target_community" {
        return Ok(None);
    }
    let Ok(target_id) = uuid::Uuid::parse_str(&unit_id) else {
        return Ok(None);
    };
    // The handle, and the evidence that the post reached the community.
    // `posted_at` is stamped when the operator registers the published URL,
    // so a row that is still `awaiting_manual_post` correctly yields nothing.
    let community: Option<String> = sqlx::query_scalar::<_, String>(
        r#"
        SELECT target.display_name
        FROM agent_outreach_targets AS target
        WHERE target.workspace_id = $1
          AND target.id = $2
          AND EXISTS (
              SELECT 1
              FROM community_posts AS post
              WHERE post.workspace_id = target.workspace_id
                AND post.target_id = target.id
                AND post.status = 'posted'
                AND post.posted_at IS NOT NULL
          )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(target_id)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx)?;
    // The unit is a community, so the workspace-level fallback would answer a
    // different question about a different population. Nothing published means
    // no outcome exists, and that is not the same fact as an outcome of zero.
    community.map_or(Err(RepositoryError::NotFound), |handle| Ok(Some(handle)))
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
pub(super) async fn refresh_evidence_readiness(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    sqlx::query(
        r#"
        UPDATE viryaos_growth_evidence AS evidence
        SET resolved_at = $3
        WHERE evidence.workspace_id = $1
          AND evidence.action_id = $2
          AND evidence.resolved_at IS NULL
          AND NOT EXISTS (
              SELECT 1
              FROM viryaos_autopilot_measurements AS outstanding
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
              FROM viryaos_experiment_assignments AS treated
              JOIN viryaos_experiment_assignments AS control
                ON control.workspace_id = treated.workspace_id
               AND control.experiment_uuid = treated.experiment_uuid
               AND control.arm = 'control'
              JOIN viryaos_growth_evidence AS control_evidence
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
    sqlx::query(
        r#"
        UPDATE viryaos_dispatch_predictions AS prediction
        SET resolved_at = $3
        WHERE prediction.workspace_id = $1
          AND prediction.action_id = $2
          AND prediction.resolved_at IS NULL
          AND NOT EXISTS (
              SELECT 1
              FROM viryaos_autopilot_measurements AS outstanding
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
    Ok(())
}

/// The evidence quality this measurement actually earned.
///
/// A randomised assignment is a claim about how the unit was chosen. It only
/// becomes randomised *evidence* when the outcome was read at the level the
/// randomisation was performed at — here, from the community's own ledger. If
/// the observation came from the workspace fallback instead, the design was
/// randomised but the reading was not, and the row says so.
pub(super) async fn measured_evidence_quality(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    measurement: &ClaimedAutopilotMeasurement,
    community: Option<&str>,
) -> Result<&'static str, RepositoryError> {
    if community.is_none() {
        return Ok("matched_quasi_experiment");
    }
    let experiment_kind: Option<String> = sqlx::query_scalar::<_, String>(
        r#"
        SELECT experiment_kind
        FROM viryaos_experiment_assignments
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
            "matched_quasi_experiment"
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
        FROM viryaos_experiment_assignments AS assignment
        JOIN viryaos_growth_evidence AS evidence
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
            now,
        )
        .await?;
    }
    Ok(())
}
