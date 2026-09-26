//! The outreach-reply execution arm: locks the target under its approved
//! version, reserves the contact window, emits the executor event and writes
//! the outbound half of the conversation so the unanswered-replies board
//! closes the loop.
//!
//! The gate set differs from a pitch on purpose: `verified` and
//! `accepts_outreach` bar a stranger being asked for something, while a reply
//! answers somebody who wrote to us. The bars that stay are the version pin,
//! `active`, and `do_not_contact` — the line that can never move.

use serde_json::json;

use super::*;
use crate::autopilot::{emit_outward_action, reserve_contact_window};
use crowdrelay_application::autopilot::{AutopilotActionPayload, ClaimedAutopilotAction};

/// Locks the reply's target for the send — the reply lane's counterpart of
/// `lock_outreach_for_execution`, without the opportunity join: a reply
/// answers a person, not an opportunity.
///
/// Returns `(display_name, contact_email, target_kind)` for the emission.
pub(in crate::autopilot) async fn lock_reply_target_for_execution(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    target_id: OutreachTargetId,
    target_version: i64,
) -> Result<(String, String, String), RepositoryError> {
    sqlx::query_as::<_, (String, String, String)>(
        r#"
        SELECT display_name, contact_email, target_kind
        FROM outreach_targets
        WHERE workspace_id = $1 AND id = $2 AND version = $3
          AND active AND NOT do_not_contact
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(target_id.into_uuid())
    .bind(target_version)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)
}

/// Records that the reply action sent — the outbound half of the
/// conversation, so the unanswered-replies board closes the loop on it.
///
/// Phase is `reply`: direction `outbound` + phase `reply` means we answered,
/// and every "is this conversation waiting on us" read keys off exactly that
/// pair. `followup_count` does not move — it counts pitch follow-ups against
/// the cadence caps, and an answer is not a pitch.
pub(in crate::autopilot) async fn record_reply_sent(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: crowdrelay_domain::AutopilotActionId,
    target_id: OutreachTargetId,
    reply_interaction_id: i64,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    sqlx::query("UPDATE outreach_targets SET last_outreach_at=$3 WHERE workspace_id=$1 AND id=$2")
        .bind(workspace_id.into_uuid())
        .bind(target_id.into_uuid())
        .bind(now)
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx)?;
    sqlx::query(
        r#"INSERT INTO outreach_interactions(workspace_id,target_id,opportunity_id,direction,phase,source_key,occurred_at,metadata)
           VALUES($1,$2,NULL,'outbound','reply',$3,$4,jsonb_build_object('answers_interaction_id', $5))
           ON CONFLICT(workspace_id,target_id,source_key) DO NOTHING"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(target_id.into_uuid())
    .bind(format!("autopilot:reply:{action_id}"))
    .bind(now)
    .bind(reply_interaction_id)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    sqlx::query(
        r#"INSERT INTO reach_events (workspace_id, action_id, recipient_kind, recipient_id, channel, template_id, estimated_reach, status, metadata)
           VALUES ($1, $2, 'outreach_target', $3::text, 'email', 'outreach.reply', 1, 'sent', jsonb_build_object('answers_interaction_id', $4))
           ON CONFLICT (action_id, recipient_id, channel) WHERE action_id IS NOT NULL DO NOTHING"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .bind(target_id.into_uuid())
    .bind(reply_interaction_id)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

/// Sends the answer the approval covered, or sends none of it.
///
/// Takes the claimed action rather than its fields, same as the gig-outreach
/// arm: the dispatcher's `match` grows by one line per kind and this module
/// is the one that knows the payload's shape. Any other payload routed here
/// is `Conflict` — a dispatcher bug must not become a letter.
pub(in crate::autopilot) async fn execute_outreach_reply(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action: &ClaimedAutopilotAction,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let AutopilotActionPayload::RequestOutreachReply {
        target_id,
        target_version,
        reply_interaction_id,
        reply_disposition,
        sheet_verdict,
        draft,
        ..
    } = &action.payload
    else {
        return Err(RepositoryError::Conflict);
    };
    // Same rule as the pitch it answers: the operator approved these exact
    // words, so an action that lost its letter is refused rather than handed
    // to something that would improvise one.
    if draft.subject.trim().is_empty() || draft.body.trim().is_empty() {
        return Err(RepositoryError::ConflictBecause(
            "outreach reply refused: this action carries no letter — the reply composes when the action is written",
        ));
    }
    let target =
        lock_reply_target_for_execution(transaction, workspace_id, *target_id, *target_version)
            .await?;
    // The contact-window key is the mailbox: a reply and a pitch to the same
    // address must not land in one day.
    reserve_contact_window(
        transaction,
        workspace_id,
        action.id,
        "outreach",
        &target.1,
        now,
        true,
    )
    .await?;
    emit_outward_action(
        transaction,
        workspace_id,
        action.id,
        "crowdrelay.outreach.reply_requested",
        format!("outreach-target:{target_id}"),
        format!(
            "reply to answered contact, target locked at version {target_version}, not do-not-contact"
        ),
        json!({
            "action_id": action.id,
            "target_id": target_id,
            "target_name": target.0,
            "target_kind": target.2,
            "contact_email": target.1,
            // Which sheet-imported answer this letter closes — the executor
            // threads it so the reply lands on the same conversation, not a
            // new one.
            "answers_interaction_id": reply_interaction_id,
            "reply_disposition": reply_disposition,
            "sheet_verdict": sheet_verdict,
            "draft": draft,
        }),
    )
    .await?;
    record_reply_sent(
        transaction,
        workspace_id,
        action.id,
        *target_id,
        *reply_interaction_id,
        now,
    )
    .await?;
    Ok(())
}
