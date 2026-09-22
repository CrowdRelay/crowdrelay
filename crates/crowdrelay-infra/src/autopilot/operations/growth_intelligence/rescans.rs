//! One-shot rescan requests an accepted `rescan` strategy proposal leaves.
//!
//! Split out of the snapshot loader for the same reason `worker_signals`
//! was: a self-contained read, and the loader lives next to the size
//! ratchet's ceiling.

use super::*;

/// The template ids with a pending rescan for this workspace.
///
/// The table belongs to a migration that may not have run yet on a fresh
/// deployment — an absent table means no rescans pending, same as an empty
/// queue. `undefined_table` (42P01) is therefore the no-rescans answer; a
/// `to_regclass` probe would work too, but it is an unscoped catalog read
/// and this crate is ratcheted on every statement naming its workspace.
pub(super) async fn load_pending_rescans(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
) -> Result<std::collections::HashSet<String>, RepositoryError> {
    match sqlx::query_scalar::<_, String>(
        // Requests older than a week are stale — a dispatch that never
        // happened cannot be reconstructed, and the evaluator expires them
        // on the next proposal for the same template.
        "SELECT template_id FROM agent_template_rescan_requests
         WHERE workspace_id = $1 AND consumed_at IS NULL
           AND created_at > now() - interval '7 days'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    {
        Ok(rows) => Ok(rows.into_iter().collect()),
        Err(e) if undefined_table(&e) => Ok(std::collections::HashSet::new()),
        Err(e) => Err(map_sqlx(e)),
    }
}

fn undefined_table(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .is_some_and(|d| d.code().as_deref() == Some("42P01"))
}
