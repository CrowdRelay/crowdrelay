//! Execution boundary for a booking outreach (§12-6).
//!
//! # All of them, or none of them
//!
//! The action names an anchor target plus up to four same-city recipients the
//! selector ranked next — "write to A, B and C" approved as one letter. Every
//! recipient is locked at its approved version and reserved with the contact
//! governor inside the caller's transaction before anything is emitted: a
//! stale version or a governor refusal on any one of them rolls the whole
//! send back, and no contact stays reserved for a letter that never left.
//!
//! The emitted payload carries the recipient set the approval named, the
//! proposed window verbatim, and `first_line_fact` — the evidence sentence
//! rendered from the `venue_evidence` row the operator approved, so the fact
//! the worker reads is the fact the approval saw. No evidence, no line: the
//! field goes out as `null` rather than a fabricated claim.

use crowdrelay_domain::venue_evidence::{EvidenceLocale, booking_evidence_line};
use crowdrelay_domain::{BookingTargetId, booking::BookingOutreachPhase};
use serde_json::json;

use super::*;
use crate::autopilot::team::crew_locale_in_tx;
use crate::autopilot::{
    emit_outward_action, lock_booking_target_for_execution, reserve_contact_window,
};
use crowdrelay_application::autopilot::{
    AutopilotActionPayload, BriefingLocale, ClaimedAutopilotAction,
};

/// Sends the letter to every named recipient, or sends it to none.
///
/// Takes the claimed action rather than its fields for the same reason the
/// gig executor does: the dispatcher's `match` stays one line per kind, and
/// the module that writes the letter is the one that knows its shape. Any
/// other payload is `Conflict` — a misrouted dispatch is a bug, and a bug
/// must not become a letter.
pub(in crate::autopilot) async fn execute_booking_outreach(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action: &ClaimedAutopilotAction,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let action_id = action.id;
    let AutopilotActionPayload::RequestBookingOutreach {
        city_id,
        target_id,
        target_version,
        target_name: _,
        score,
        phase,
        proposed_window,
        additional_recipients,
        venue_evidence,
        draft,
    } = &action.payload
    else {
        return Err(RepositoryError::Conflict);
    };
    // The refusal that keeps the promise: an action persisted before the
    // letter travelled in the payload decodes to an empty draft, and nothing
    // may compose one here — the words the operator approved are the only
    // words a target can receive.
    if draft.subject.trim().is_empty() || draft.body.trim().is_empty() {
        return Err(RepositoryError::ConflictBecause(
            "booking outreach refused: no approved letter in the payload",
        ));
    }
    let city_id = *city_id;
    let phase_str = match phase {
        BookingOutreachPhase::Initial => "initial",
        BookingOutreachPhase::FollowUp => "followup",
    };

    // Anchor first — the letter exists because of it — then every additional
    // recipient under the same city. Lock and reserve each in turn: any
    // version that moved since approval, or any contact the governor refuses,
    // aborts the transaction and nobody stays reserved.
    let anchor = lock_booking_target_for_execution(
        transaction,
        workspace_id,
        city_id,
        *target_id,
        *target_version,
    )
    .await?;
    let mut recipients: Vec<(BookingTargetId, i64, (String, String, String))> =
        vec![(*target_id, *target_version, anchor.clone())];
    for (extra_id, extra_version) in additional_recipients {
        let locked = lock_booking_target_for_execution(
            transaction,
            workspace_id,
            city_id,
            *extra_id,
            *extra_version,
        )
        .await?;
        recipients.push((*extra_id, *extra_version, locked));
    }
    let mut addressed = Vec::with_capacity(recipients.len());
    for (recipient_id, _, locked) in &recipients {
        reserve_contact_window(
            transaction,
            workspace_id,
            action_id,
            "booking_opportunity",
            &locked.2,
            now,
        )
        .await?;
        addressed.push(json!({
            "target_id": recipient_id,
            "target_name": locked.1,
            "contact_email": locked.2,
        }));
    }

    // The first line is the fact the operator approved — rendered from the
    // evidence row carried on the action, in the crew's own language. An
    // evidenceless room sends no line rather than an invented one.
    let locale = match crew_locale_in_tx(transaction, workspace_id).await {
        BriefingLocale::Pl => EvidenceLocale::Pl,
        _ => EvidenceLocale::En,
    };
    let first_line_fact = venue_evidence
        .as_ref()
        .and_then(|evidence| booking_evidence_line(evidence, locale));

    let (anchor_kind, anchor_name, anchor_email) = &anchor;
    emit_outward_action(
        transaction,
        workspace_id,
        action_id,
        "crowdrelay.booking.outreach_requested",
        format!("booking-target:{target_id}"),
        format!(
            "screened booking target, {phase:?} phase, score {score} — locked under version {target_version}"
        ),
        json!({
            "action_id": action_id,
            "city_id": city_id,
            "target_id": target_id,
            "target_kind": anchor_kind,
            "target_name": anchor_name,
            "contact_email": anchor_email,
            "recipients": addressed,
            "proposed_window": proposed_window,
            "venue_evidence": venue_evidence,
            "first_line_fact": first_line_fact,
            "draft": draft,
            "template_key": match phase {
                BookingOutreachPhase::Initial => "booking.opportunity.v1",
                BookingOutreachPhase::FollowUp => "booking.followup.v1",
            },
            "phase": phase,
            "score": score,
        }),
    )
    .await?;

    // One outbound interaction per recipient — a reply still arrives from a
    // person, and the learning loop needs each silence attributed to its own
    // target rather than to the batch.
    for (recipient_id, expected_version, _) in &recipients {
        let changed = sqlx::query(
            r#"
            UPDATE booking_targets
            SET last_outreach_at = $4
            WHERE workspace_id = $1 AND id = $2 AND version = $3
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(recipient_id.into_uuid())
        .bind(expected_version)
        .bind(now)
        .execute(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
        if changed.rows_affected() != 1 {
            return Err(RepositoryError::Conflict);
        }
        sqlx::query(
            r#"
            INSERT INTO booking_interactions(
                workspace_id,target_id,direction,phase,source_key,occurred_at,metadata
            ) VALUES($1,$2,'outbound',$3,$4,$5,jsonb_build_object('action_id',$6::uuid))
            ON CONFLICT(workspace_id,target_id,source_key) DO NOTHING
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(recipient_id.into_uuid())
        .bind(phase_str)
        .bind(format!("autopilot:{action_id}"))
        .bind(now)
        .bind(action_id.into_uuid())
        .execute(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
    }
    Ok(())
}
