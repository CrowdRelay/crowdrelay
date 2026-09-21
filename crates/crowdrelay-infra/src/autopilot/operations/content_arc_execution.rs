//! Execution boundary for content arcs.
//!
//! Approving an arc ask is the season's one creative decision: the band
//! commits to the shape, and every beat inside the spine then surfaces under
//! that answer instead of asking again. The only write here is the
//! `proposed → approved` transition nobody else may make — the arc goes
//! `active` on its own when its horizon opens, inside the refresh sweep.

use super::*;

pub(in crate::autopilot) async fn approve_content_arc(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    arc_id: crowdrelay_domain::ArcId,
) -> Result<(), RepositoryError> {
    // The approver's name and click time ride the join: the arc records who
    // said yes and when, and the source of truth for both is the action row
    // the operator signed — not the moment the executor got around to it.
    let changed = sqlx::query(
        r#"
        UPDATE arcs AS arc
        SET status = 'approved',
            approved_at = COALESCE(action.approved_at, now()),
            approved_by = action.approved_by,
            updated_at = now()
        FROM autopilot_actions AS action
        WHERE action.workspace_id = $1 AND action.id = $3
          AND arc.workspace_id = $1 AND arc.id = $2 AND arc.status = 'proposed'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(arc_id.into_uuid())
    .bind(action_id.into_uuid())
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .rows_affected();
    if changed == 0 {
        // A replay finds the row already approved or active — that is the
        // answer, not a failure. Anything else means the queue entry
        // outlived its question: a retired arc must not be resurrected by
        // a stale approval.
        let status = sqlx::query_scalar::<_, String>(
            "SELECT status FROM arcs WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id.into_uuid())
        .bind(arc_id.into_uuid())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
        match status.as_deref() {
            Some("approved" | "active" | "completed") => Ok(()),
            Some(_) => Err(RepositoryError::ConflictBecause(
                "This arc was already retired or superseded — approving it now would reopen a season the band closed.",
            )),
            None => Err(RepositoryError::NotFound),
        }
    } else {
        Ok(())
    }
}
