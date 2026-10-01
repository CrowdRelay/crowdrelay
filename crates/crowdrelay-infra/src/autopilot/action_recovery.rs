//! Fence execution by its durable attempt and account for interrupted claims.

use super::*;

pub(super) async fn lock_current_attempt(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action: &ClaimedAutopilotAction,
) -> Result<(), RepositoryError> {
    let current = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id FROM autopilot_actions
        WHERE workspace_id = $1 AND id = $2
          AND status = 'processing' AND attempt_count = $3
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action.id.into_uuid())
    .bind(i32::try_from(action.attempt_number).map_err(|_| RepositoryError::Unexpected)?)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    current.ok_or(RepositoryError::Conflict)?;
    Ok(())
}

/// Candidates are already locked by the claim transaction. Only stale
/// processing rows have an interrupted attempt; queued retries were closed
/// by fail_action. Closing and re-claiming commit together.
pub(super) async fn close_interrupted_attempts(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    runnable: &[Uuid],
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    sqlx::query(
        r#"
        INSERT INTO autopilot_action_attempts (
            workspace_id, action_id, attempt_number, outcome, error_kind, occurred_at
        )
        SELECT action.workspace_id, action.id, action.attempt_count,
               'failed', 'stale_claim_recovered', $3
        FROM autopilot_actions AS action
        WHERE action.workspace_id = $1 AND action.id = ANY($2)
          AND action.status = 'processing'
          AND action.attempt_count > 0
          AND NOT EXISTS (
              SELECT 1 FROM autopilot_action_attempts AS terminal
              WHERE terminal.workspace_id = action.workspace_id
                AND terminal.action_id = action.id
                AND terminal.attempt_number = action.attempt_count
                AND terminal.outcome IN ('succeeded', 'failed')
          )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(runnable)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}
