//! Persistence for the brain's belief-revision ledger.
//!
//! Append-only. Nothing updates or deletes a revision, and nothing in the
//! brain reads one back — see `viryaos_brain_belief_revisions` in migration
//! 0252 for why the ledger exists at all.

use crowdrelay_application::{RepositoryError, autopilot::BeliefRevision};
use crowdrelay_domain::WorkspaceId;
use uuid::Uuid;

use super::{PostgresAutopilotRepository, map_sqlx};

/// Appends belief revisions for one workspace.
///
/// Unattributable revisions are dropped rather than written: the ledger's
/// whole claim is "this belief moved because of that action", and a row that
/// cannot name the action makes the claim unverifiable.
pub(in crate::autopilot) async fn record_belief_revisions(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    revisions: &[BeliefRevision],
) -> Result<(), RepositoryError> {
    let pool = &repo.pool;
    for revision in revisions.iter().filter(|r| r.is_attributable()) {
        sqlx::query(
            r#"
            INSERT INTO viryaos_brain_belief_revisions (
                id, workspace_id, module, belief_key,
                previous_value, current_value, change_summary,
                caused_by_action_ids
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id.into_uuid())
        .bind(revision.module.as_str())
        .bind(&revision.belief_key)
        .bind(&revision.previous_value)
        .bind(&revision.current_value)
        .bind(&revision.change_summary)
        .bind(&revision.caused_by_action_ids)
        .execute(pool)
        .await
        .map_err(map_sqlx)?;
    }
    Ok(())
}
