//! Execution boundary for a band-approved gig outreach (4G.4).
//!
//! # All of them, or none of them
//!
//! A gig proposal names everybody who books one room. §12-6 makes that one
//! action with a recipient set rather than one action each, and this is where
//! that matters: every recipient is locked and reserved with the contact
//! governor before anything is emitted, and all of it is the caller's
//! transaction. A promoter the governor refuses — a do-not-contact, a cooldown,
//! a sibling act in the same organisation holding the window — takes the whole
//! letter down with them.
//!
//! The alternative was measured in the design and rejected: three promoters who
//! book the same city talk to each other, and writing to two of them about one
//! night reads as a snub or a shambles depending on who compares notes. Nobody
//! hearing is recoverable in a sentence. Two of three hearing is not.
//!
//! Nothing the approval wrote is trusted past the moment it was checked: the
//! row lock re-pins each target's version, so a promoter edited between the
//! approval and the send fails rather than being written to under stale terms.

use serde_json::json;

use super::*;
use crate::autopilot::{
    emit_outward_action, lock_booking_target_for_execution, reserve_contact_window,
};
use crowdrelay_application::autopilot::{
    AutopilotActionPayload, ClaimedAutopilotAction, GigLetterKind,
};

/// Writes the letter, or writes none of it.
///
/// Takes the claimed action rather than its fields: the dispatcher's `match` is one
/// read of every payload in the system and grows by one line per kind, while
/// the module that writes the letter is the one that should know its shape.
/// Any other payload is `Conflict` — a dispatcher that routed the wrong kind
/// here is a bug, and a bug must not become a letter.
pub(in crate::autopilot) async fn execute_gig_outreach(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action: &ClaimedAutopilotAction,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let action_id = action.id;
    let AutopilotActionPayload::RequestGigOutreach {
        city_id,
        venue,
        recipients,
        opening_line,
        reasons,
        letter,
        draft,
    } = &action.payload
    else {
        return Err(RepositoryError::Conflict);
    };
    let city_id = *city_id;
    // O.1: the letter is composed at approval time and travels in the payload.
    // A row queued before that — or one whose draft was lost — is refused here
    // rather than passed to an executor that would compose its own words. The
    // band approved sentences; sending different ones is the failure this whole
    // path exists to prevent.
    if draft.subject.trim().is_empty() || draft.body.trim().is_empty() {
        return Err(RepositoryError::ConflictBecause(
            "gig outreach refused: this action carries no letter — it was queued before the letter was composed at approval time, and nothing may write one on the band's behalf now. Approve the proposal again to compose it.",
        ));
    }
    let mut addressed = Vec::with_capacity(recipients.len());
    for recipient in recipients {
        let target = lock_booking_target_for_execution(
            transaction,
            workspace_id,
            city_id,
            recipient.target_id,
            recipient.target_version,
        )
        .await?;
        reserve_contact_window(
            transaction,
            workspace_id,
            action_id,
            "gig_outreach",
            &target.2,
            now,
        )
        .await?;
        addressed.push(json!({
            "target_id": recipient.target_id,
            "target_kind": target.0,
            "target_name": target.1,
            "contact_email": target.2,
        }));
    }

    // The room's status is re-resolved at send time, not trusted from the
    // decision: a closure announced between approval and send — the
    // registry learns it through the next sweep or seed sheet — must take
    // the letter down rather than let a promoter read a proposal for a
    // night at a dead room. Only the winning claim decides, so a newer
    // 'active' lifts a stale 'closed' exactly as `best_venue` reads it.
    // A name that is not a registry row carries no verdict and passes —
    // support-slot asks name an event's venue, which the registry may
    // never have seen.
    let named_room_closed = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT COALESCE((
            SELECT lower(btrim(status_fact.value))
            FROM place_venue_facts AS status_fact
            WHERE status_fact.venue_id = venue.id
              AND status_fact.attribute = 'status'
              AND (status_fact.workspace_id IS NULL
                   OR status_fact.workspace_id = $1)
              AND (status_fact.expires_at IS NULL
                   OR status_fact.expires_at > now())
            ORDER BY CASE status_fact.provenance
                         WHEN 'played' THEN 0 WHEN 'researched' THEN 1
                         WHEN 'event_evidence' THEN 2
                         WHEN 'open_directory' THEN 3
                         ELSE 4 END,
                     status_fact.observed_at DESC
            LIMIT 1
        ), '') = 'closed'
        FROM place_venues AS venue
        WHERE venue.city_id = $2
          AND venue.name_key = place_venue_key($3)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(city_id.into_uuid())
    .bind(venue)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .unwrap_or(false);
    if named_room_closed {
        return Err(RepositoryError::ConflictBecause(
            "gig outreach refused: the room this letter names is on record as closed — a closure reported after the proposal was made must take the letter down rather than let a promoter read a pitch for a night at a dead room.",
        ));
    }

    // The template key is part of which letter this is — a support-slot ask
    // must not leave wearing `gig.proposal.v1`, because the executor renders
    // the named template verbatim and a proposal letter would announce a night
    // that is already booked.
    let (support_act, event_id, show_date, source_id, recipient_reason) = match letter {
        GigLetterKind::Proposal => (
            None,
            None,
            None,
            format!("gig-proposal:{city_id}"),
            "every promoter the band-approved proposal named — locked under \
             version, contact window reserved"
                .to_owned(),
        ),
        GigLetterKind::SupportSlotAsk {
            support_act,
            event_id,
            show_date,
        } => (
            Some(support_act.clone()),
            Some(event_id.into_uuid()),
            Some(show_date.clone()),
            format!("support-slot-ask:{event_id}"),
            "the headliner's own promoter for the show holding the slot — \
             locked under version, contact window reserved"
                .to_owned(),
        ),
    };
    emit_outward_action(
        transaction,
        workspace_id,
        action_id,
        "crowdrelay.gig.outreach_requested",
        source_id,
        recipient_reason,
        json!({
            "action_id": action_id,
            "city_id": city_id,
            "venue": venue,
            "template_key": letter.template_key(),
            "letter": letter,
            "support_act": support_act,
            "event_id": event_id,
            "show_date": show_date,
            "opening_line": opening_line,
            "reasons": reasons,
            // The letter itself, approved word for word. `draft` is the name
            // the outward gate's identical-draft check and
            // `draft_revision::revisable_fields` both already look for.
            "draft": draft,
            "recipients": addressed,
        }),
    )
    .await?;

    for recipient in recipients {
        let changed = sqlx::query(
            r#"
            UPDATE booking_targets
            SET last_outreach_at = $4
            WHERE workspace_id = $1 AND id = $2 AND version = $3
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(recipient.target_id.into_uuid())
        .bind(recipient.target_version)
        .bind(now)
        .execute(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
        if changed.rows_affected() != 1 {
            return Err(RepositoryError::Conflict);
        }
        // One interaction per promoter, not one per letter: the reply that
        // matters later is from a person, and 4G.5 asks which kind of evidence
        // predicts a booking — which it cannot answer if three promoters'
        // silence arrives as one row.
        sqlx::query(
            r#"
            INSERT INTO booking_interactions(
                workspace_id,target_id,direction,phase,source_key,occurred_at,metadata
            ) VALUES($1,$2,'outbound','initial',$3,$4,
                     jsonb_build_object('action_id',$5::uuid,'gig_outreach',true))
            ON CONFLICT(workspace_id,target_id,source_key) DO NOTHING
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(recipient.target_id.into_uuid())
        .bind(format!("autopilot:{action_id}"))
        .bind(now)
        .bind(action_id.into_uuid())
        .execute(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
    }
    Ok(())
}
