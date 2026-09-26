//! The booking-agent reply execution arm: locks the answered agent under
//! the approved version, reserves the contact window, emits the executor
//! event and writes the outbound half of the conversation so the agents
//! board's "waiting on you" closes the loop.
//!
//! Same rules as the outreach reply arm with the agent lane's gate: the
//! season spend and the draw floor do not apply — the reply spends nothing,
//! it answers the season's ask — while the version pin, `active`,
//! `do_not_contact` and the confirmed route hold exactly as they do for the
//! approach. The draft is refused empty for the same reason every letter
//! here is: nobody writes on the band's behalf after the approval.

use serde_json::json;

use super::*;
use crate::autopilot::{emit_outward_action, reserve_contact_window};
use crowdrelay_application::autopilot::{AutopilotActionPayload, ClaimedAutopilotAction};

/// Sends the answer the approval covered, or sends none of it.
///
/// Takes the claimed action rather than its fields, same as the other
/// execution arms: the dispatcher's `match` grows by one line per kind and
/// this module is the one that knows the payload's shape. Any other payload
/// routed here is `Conflict`.
pub(in crate::autopilot) async fn execute_booking_agent_reply(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action: &ClaimedAutopilotAction,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let AutopilotActionPayload::RequestBookingAgentReply {
        agent_id,
        agent_version,
        agency,
        reply_interaction_id,
        reply_disposition,
        draft,
        ..
    } = &action.payload
    else {
        return Err(RepositoryError::Conflict);
    };
    // Same refusal as the approach and the outreach reply: the operator
    // approved these exact words, so an action that lost its letter is
    // refused rather than handed to anything that would improvise one.
    if draft.subject.trim().is_empty() || draft.body.trim().is_empty() {
        return Err(RepositoryError::ConflictBecause(
            "booking-agent reply refused: this action carries no letter — the reply composes when the draft is requested",
        ));
    }
    let agent = crate::booking_agents::lock_agent_reply_for_execution(
        transaction,
        workspace_id,
        *agent_id,
        *agent_version,
    )
    .await?;
    // The contact-window key is the mailbox: an answer and an approach to
    // the same address must not land in one day.
    reserve_contact_window(
        transaction,
        workspace_id,
        action.id,
        "booking_agent",
        &agent.contact_email,
        now,
        true,
    )
    .await?;
    emit_outward_action(
        transaction,
        workspace_id,
        action.id,
        "crowdrelay.booking_agent.reply_requested",
        format!("booking-agent:{agent_id}"),
        "reply to an agent who wrote back — locked under version, route confirmed",
        json!({
            "action_id": action.id,
            "agent_id": agent_id,
            "agent_name": agent.name,
            "agency": agent.agency.or_else(|| agency.clone()),
            "contact_email": agent.contact_email,
            // Which filed answer this letter closes — the executor threads
            // it so the reply lands on the same conversation, not a new one.
            "answers_interaction_id": reply_interaction_id,
            "reply_disposition": reply_disposition,
            // Approved word for word — the executor sends `draft.body`
            // verbatim.
            "draft": draft,
        }),
    )
    .await?;
    crate::booking_agents::record_agent_reply_sent(
        transaction,
        workspace_id,
        action.id,
        *agent_id,
        *reply_interaction_id,
        now,
    )
    .await?;
    Ok(())
}
