//! The booking-agent approach-wave execution arm: every approach the approval
//! covered re-runs its own gates inside the wave's transaction — the version
//! lock, the contact window, the re-measured draw evidence — and one moved
//! gate fails the wave rather than sending the rest of a batch the approval
//! never priced separately.

use serde_json::json;

use super::*;
use crate::autopilot::{emit_outward_action_keyed, reserve_contact_window, send_evidence};
use crowdrelay_application::autopilot::{AutopilotActionPayload, ClaimedAutopilotAction};

/// Sends every letter the wave card covered, or none of them.
///
/// Takes the claimed action rather than its fields, same as the
/// gig-outreach arm: the dispatcher's `match` grows by one line per kind
/// and this module is the one that knows the payload's shape. Any other
/// payload routed here is `Conflict`.
pub(in crate::autopilot) async fn execute_booking_agent_approach_wave(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action: &ClaimedAutopilotAction,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let AutopilotActionPayload::RequestBookingAgentApproachWave {
        wave_id,
        note,
        approaches,
        ..
    } = &action.payload
    else {
        return Err(RepositoryError::Conflict);
    };
    for approach in approaches {
        if approach.draft.subject.trim().is_empty() || approach.draft.body.trim().is_empty() {
            return Err(RepositoryError::ConflictBecause(
                "booking-agent wave refused: an approach carries no letter — \
                 approve the wave again to compose it",
            ));
        }
        let agent = crate::booking_agents::lock_agent_for_execution(
            transaction,
            workspace_id,
            approach.agent_id,
            approach.agent_version,
            now,
        )
        .await?;
        reserve_contact_window(
            transaction,
            workspace_id,
            action.id,
            "booking_agent",
            &agent.contact_email,
            now,
        )
        .await?;
        // One letter per agent means one emission per agent — the agent's
        // id keys the emission so approaches 2..N are not swallowed by the
        // shared `autopilot-action:{id}` default key, and a dispatch retry
        // re-targets the same rows rather than double-sending.
        emit_outward_action_keyed(
            transaction,
            workspace_id,
            action.id,
            "crowdrelay.booking_agent.approach_requested",
            send_evidence(
                format!("booking-agent:{}", approach.agent_id),
                "wave-approved booking agent — locked under version, draw evidence re-measured at dispatch",
            )?,
            json!({
                "action_id": action.id,
                "wave_id": wave_id,
                "agent_id": approach.agent_id,
                "agent_name": agent.name,
                "agency": agent.agency,
                "contact_email": agent.contact_email,
                "note": note,
                "evidence": agent.evidence,
                // Approved word for word — the executor sends `draft.body`
                // verbatim.
                "draft": approach.draft,
            }),
            &format!("agent:{}", approach.agent_id),
        )
        .await?;
        crate::booking_agents::record_approach_sent(
            transaction,
            workspace_id,
            action.id,
            approach.agent_id,
            now,
        )
        .await?;
    }
    Ok(())
}
