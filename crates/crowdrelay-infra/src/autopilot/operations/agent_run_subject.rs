//! Who an agent run is about, read from the action that dispatched it.
//!
//! Split out of `execute_agent_run` for the source-size ratchet, and because it
//! is one question with one answer: the dispatching action's trace, and — when
//! the action is about a beacon — that beacon and the address it is reached at.
//!
//! Internal relationship research is bound to the action's subject, never to
//! anything the model says. The beacon id stays audit-only; the agents tool
//! receives the address and never returns it to the model.

use super::*;

pub(super) struct RunSubject {
    pub trace_id: Option<uuid::Uuid>,
    pub beacon_id: Option<uuid::Uuid>,
    pub contact_email: Option<String>,
}

pub(super) async fn resolve_run_subject(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
) -> Result<RunSubject, RepositoryError> {
    let (trace_id, subject_kind, subject_id) =
        sqlx::query_as::<_, (Option<uuid::Uuid>, String, uuid::Uuid)>(
            r#"
            SELECT trace_id, subject_kind, subject_id
            FROM autopilot_actions
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(action_id.into_uuid())
        .fetch_optional(&mut **tx)
        .await
        .map_err(map_sqlx)?
        .ok_or(RepositoryError::Conflict)?;

    let beacon_id = (subject_kind == "beacon").then_some(subject_id);
    let contact_email = if beacon_id.is_some() {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT lower(btrim(contact_email)) FROM beacons \
             WHERE workspace_id = $1 AND id = $2 AND active",
        )
        .bind(workspace_id.into_uuid())
        .bind(subject_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(map_sqlx)?
        .flatten()
        .filter(|email| !email.is_empty())
    } else {
        None
    };
    Ok(RunSubject {
        trace_id,
        beacon_id,
        contact_email,
    })
}
