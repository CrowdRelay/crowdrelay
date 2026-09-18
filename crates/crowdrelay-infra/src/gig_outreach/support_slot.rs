//! The support-slot ask (N.5).
//!
//! The cheapest gig in the system is a confirmed show with room on the bill —
//! `open_support_slots` reads them, `plan_roster_run` ranks filling one above
//! booking a new night, and this is the write half: the roster operator names
//! the headliner and the labelmate, and the headliner's own promoter gets one
//! letter asking to confirm the support act for the slot they offered.
//!
//! The differences from [`super::approve_gig_proposal`] are the honest ones.
//! The ask lives on the headliner's workspace — the room, the promoter
//! relationship and the night are theirs, and a letter from the label's
//! account would be a stranger writing to somebody else's booker. There is no
//! proposal to replay: the plan named an act, and the gates below recompute
//! whether that act still fills this slot rather than re-asking the planner
//! who it would pick — the operator's choice is the input, the evidence is
//! the constraint.
//!
//! The approve-with-edit path (N.10) is the band path's own: the operator's
//! fix to the opening line is reviewed against the composed letter before
//! anything is written, a refused revision refuses the approval and spends
//! nothing, and an accepted one lands in the payload and on the ledger.

use std::collections::BTreeMap;

use crowdrelay_application::IdempotencyKey;
use crowdrelay_application::autopilot::{
    AutopilotActionPayload, GigLetterKind, GigOutreachRecipient,
};
use crowdrelay_domain::roster_plan::{
    MINIMUM_SLOT_LEAD_DAYS, PAIRING_OVERLAP_CEILING_BASIS_POINTS,
};
use crowdrelay_domain::trace::TraceContext;
use crowdrelay_domain::{BookingTargetId, CityId, EventId, WorkspaceId};
use serde_json::json;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    AppliedRevision, GigOutreachError, QueueOutcome, SEND_CHANNEL_MISSING, apply_operator_revision,
    existing_action, gig_outreach_is_sendable, inflight_status, record_approval, record_revision,
};
use crate::gig_planning::promoter_targets_in_city;
use crate::place_reach::{audience_overlaps_by_city, reachable_in_city};

/// What `approve_support_slot_ask` did.
///
/// `Queued` names what the operator approved — the two acts, the night, the
/// room and everybody who will receive the letter — so the response is what
/// the screen shows, not a row id to look up. `Replayed` is the same contract
/// as the band path: the idempotency key already produced this letter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SupportSlotAskOutcome {
    Queued {
        action_id: Uuid,
        /// The act whose show holds the slot — the workspace the letter
        /// leaves from.
        headliner: String,
        /// The labelmate being put forward for the slot.
        support: String,
        /// The city's display name, as the letter reads it.
        city: String,
        venue: String,
        /// The night as the letter states it — "14 Nov 2026".
        show_date: String,
        /// The headliner's own promoter contacts in that city. All of them,
        /// or none — the same all-or-none reservation as the proposal letter.
        recipients: Vec<String>,
        opening_line: String,
    },
    Replayed {
        action_id: Uuid,
        status: String,
    },
}

/// The soonest published show of `headliner` in this city that still has a
/// declared slot. Recomputed from `events` rather than trusted from the
/// request: a slot filled, a show cancelled or an offer never logged all read
/// the same way — there is nothing to ask about.
#[derive(Debug, sqlx::FromRow)]
struct OpenSlotRow {
    event_id: Uuid,
    venue: String,
    starts_at: OffsetDateTime,
    city_name: String,
    days_until_show: i64,
    open_slots: i16,
}

async fn open_slot_for(
    pool: &PgPool,
    headliner_workspace_id: Uuid,
    city_id: Uuid,
    now: OffsetDateTime,
) -> Result<Option<OpenSlotRow>, sqlx::Error> {
    sqlx::query_as::<_, OpenSlotRow>(
        r#"
        SELECT event.id AS event_id,
               COALESCE(NULLIF(btrim(event.venue), ''), 'the room') AS venue,
               event.starts_at,
               city.name AS city_name,
               FLOOR(EXTRACT(EPOCH FROM (event.starts_at - $3)) / 86400)::bigint
                   AS days_until_show,
               event.open_support_slots AS open_slots
        FROM events AS event
        JOIN cities AS city ON city.id = event.city_id
        WHERE event.workspace_id = $1
          AND event.city_id = $2
          AND event.status = 'published'
          AND event.starts_at > $3
          AND event.open_support_slots > 0
        ORDER BY event.starts_at
        LIMIT 1
        "#,
    )
    .bind(headliner_workspace_id)
    .bind(city_id)
    .bind(now)
    .fetch_optional(pool)
    .await
}

/// Approves one open slot producing one letter to the headliner's promoter.
///
/// `revision` is the same approve-with-edit path the band's proposal takes:
/// the operator's fix to the letter's opening line, reviewed against the
/// composed payload before anything is written.
///
/// # Errors
///
/// `NotFound` when either workspace is not on this roster — the ask names
/// something that is not on the board. `Refused` when the slot is gone, the
/// support fills nothing, the pairing splits one crowd, the headliner cannot
/// send, nobody books the city, an ask for this show is already in flight, or
/// the revision is refused. Database errors propagate.
#[allow(clippy::too_many_arguments)]
pub async fn approve_support_slot_ask(
    pool: &PgPool,
    organization_id: Uuid,
    headliner_workspace_id: Uuid,
    support_workspace_id: Uuid,
    city_id: Uuid,
    idempotency_key: &IdempotencyKey,
    now: OffsetDateTime,
    revision: Option<&BTreeMap<String, String>>,
) -> Result<SupportSlotAskOutcome, GigOutreachError> {
    // Replay first, same rule as the band path: a retried approval answers
    // from the ledger, not from evidence that may have moved since.
    if let Some((action_id, status)) =
        existing_action(pool, headliner_workspace_id, idempotency_key).await?
    {
        return Ok(SupportSlotAskOutcome::Replayed { action_id, status });
    }

    // The letter goes out under the headliner's name to the headliner's
    // promoter — an act cannot open for itself, and neither side may be a
    // workspace the organisation does not own. Names come back with the ids
    // because the letter and the refusal sentences both read them.
    if headliner_workspace_id == support_workspace_id {
        return Err(GigOutreachError::Refused(
            "an act cannot open for itself — the slot is already theirs".to_owned(),
        ));
    }
    let members = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, name FROM workspaces WHERE organization_id = $1 AND id = ANY($2)",
    )
    .bind(organization_id)
    .bind([headliner_workspace_id, support_workspace_id])
    .fetch_all(pool)
    .await?;
    let name_of = |id: Uuid| {
        members
            .iter()
            .find(|(member_id, _)| *member_id == id)
            .map(|(_, name)| name.clone())
    };
    let (Some(headliner), Some(support)) = (
        name_of(headliner_workspace_id),
        name_of(support_workspace_id),
    ) else {
        return Err(GigOutreachError::NotFound);
    };

    // The slot is read fresh: a screen that said "open" an hour ago is not the
    // bill today. Soonest first when more than one show has room — that is the
    // night the roster meant.
    let Some(slot) = open_slot_for(pool, headliner_workspace_id, city_id, now).await? else {
        return Err(GigOutreachError::Refused(format!(
            "{headliner} has no published show with an open slot in this city right now — \
             the slot may have been filled, the show may have moved, or the offer was never \
             logged. Declared slots only: the planner does not infer room on a bill"
        )));
    };
    if slot.days_until_show < i64::from(MINIMUM_SLOT_LEAD_DAYS) {
        return Err(GigOutreachError::Refused(format!(
            "the show at {} is {} days out — a support ask needs at least \
             {MINIMUM_SLOT_LEAD_DAYS} for an answer, a rehearsal and a van. That \
             call is the promoter's to make by phone, not a letter's",
            slot.venue, slot.days_until_show
        )));
    }

    // What the support brings. `None` is an unmeasurable city — a different
    // answer from a counted zero, and the refusal says which it is. A support
    // that reaches nobody here is a name on a poster that adds nothing, which
    // spends the promoter's favour without filling the room.
    let reachable = match reachable_in_city(pool, support_workspace_id, city_id).await? {
        Some(0) => {
            return Err(GigOutreachError::Refused(format!(
                "{support} reaches nobody around {} that we can count — asking for the \
                 slot spends a favour on a name that brings nobody to the night",
                slot.city_name
            )));
        }
        Some(reachable) => reachable,
        None => {
            return Err(GigOutreachError::Refused(format!(
                "we cannot measure {support}'s audience around {} — the city has no \
                 coordinates on record, so there is nothing to measure the ask against",
                slot.city_name
            )));
        }
    };

    // The pairing arithmetic is the same `choose_support` decides on: a share
    // of the *support's* audience that is already the headliner's, in this
    // city. Past the ceiling the slot puts one crowd on the poster twice and
    // splits the door. An absent pair means the headliner reaches nobody
    // measured here — `shared` is then zero, and every person the support
    // brings is new.
    let overlaps = audience_overlaps_by_city(
        pool,
        &[headliner_workspace_id, support_workspace_id],
        &[city_id],
    )
    .await?;
    let pair = overlaps.iter().find(|overlap| {
        overlap.city_id == city_id
            && (overlap.workspace_a == headliner_workspace_id
                || overlap.workspace_b == headliner_workspace_id)
            && (overlap.workspace_a == support_workspace_id
                || overlap.workspace_b == support_workspace_id)
    });
    let shared = pair.map_or(0, |overlap| overlap.shared);
    if let Some(share) = pair.and_then(|overlap| overlap.share_of(support_workspace_id))
        && share > PAIRING_OVERLAP_CEILING_BASIS_POINTS
    {
        return Err(GigOutreachError::Refused(format!(
            "of the {reachable} people {support} reaches around {}, {}% already \
             follow {headliner} — the same crowd twice is a door split, not a \
             bigger room",
            slot.city_name,
            share / 100
        )));
    }
    let adds = reachable.saturating_sub(shared);

    // Nothing queued may park: if the headliner's workspace has no sender that
    // advertises gig outreach, the approval would sit `queued` until a sweep
    // cancels it a day later, and the operator would have said yes to a letter
    // nobody received. Same gate, same sentence as the band path.
    if !gig_outreach_is_sendable(pool, headliner_workspace_id).await? {
        return Err(GigOutreachError::Refused(SEND_CHANNEL_MISSING.to_owned()));
    }

    // Everybody who books this city for the headliner — the ask is theirs to
    // answer, and the all-or-none reservation is the same one the proposal
    // letter makes: these promoters talk to each other.
    let recipients = promoter_targets_in_city(pool, headliner_workspace_id, city_id).await?;
    if recipients.is_empty() {
        return Err(GigOutreachError::Refused(format!(
            "the slot is real but {headliner} has no contactable promoter in {} to \
             put the name to — check the booking list before writing",
            slot.city_name
        )));
    }

    // One open slot, one letter. The subject is the show, not the city — a
    // second ask while the first is unanswered is the same letter, and the
    // ledger's in-flight index would refuse the write anyway. Asked late for
    // the same reason as the band path: a slot that stopped existing has a
    // better answer than this one.
    if let Some(status) = inflight_status(pool, headliner_workspace_id, slot.event_id).await? {
        return Err(GigOutreachError::Refused(format!(
            "an ask for this show is already {status}. Cancel it on the operations \
             board if the slot should name a different act"
        )));
    }

    let show_date = slot
        .starts_at
        .format(time::macros::format_description!(
            "[day] [month repr:short] [year]"
        ))
        .unwrap_or_else(|_| slot.starts_at.date().to_string());
    let (opening_line, reasons) = support_slot_ask_letter(
        &headliner,
        &support,
        &slot.city_name,
        &slot.venue,
        &show_date,
        reachable,
        pair.map(|overlap| overlap.shared),
        adds,
    );
    // O.1: the ask's letter is composed here too, so the roster operator reads
    // the same words the headliner's promoter will.
    let sender = super::sender_identity(pool, headliner_workspace_id).await?;
    let draft = crowdrelay_domain::gig_letter::compose_letter(
        &crowdrelay_domain::gig_letter::LetterInput {
            kind: crowdrelay_domain::gig_letter::LetterKind::SupportSlotAsk,
            sender: &sender,
            venue: &slot.venue,
            opening_line: &opening_line,
            reasons: &reasons,
            support_act: Some(&support),
            show_date: Some(&show_date),
        },
    )
    .map_err(|refusal| GigOutreachError::Refused(refusal.message().to_owned()))?;

    let payload = AutopilotActionPayload::RequestGigOutreach {
        city_id: CityId::from_uuid(city_id),
        venue: slot.venue.clone(),
        recipients: recipients
            .iter()
            .map(|target| GigOutreachRecipient {
                target_id: BookingTargetId::from_uuid(target.target_id),
                target_version: target.target_version,
                target_name: target.name.clone(),
            })
            .collect(),
        opening_line: opening_line.clone(),
        reasons: reasons.clone(),
        draft,
        letter: GigLetterKind::SupportSlotAsk {
            support_act: support.clone(),
            event_id: EventId::from_uuid(slot.event_id),
            show_date: show_date.clone(),
        },
    };
    let input_snapshot = json!({
        "organization_id": organization_id,
        "headliner": headliner,
        "headliner_workspace_id": headliner_workspace_id,
        "support": support,
        "support_workspace_id": support_workspace_id,
        "event_id": slot.event_id,
        "city": slot.city_name,
        "city_id": city_id,
        "venue": slot.venue,
        "starts_at": slot.starts_at,
        "show_date": show_date,
        "open_slots": slot.open_slots,
        "days_until_show": slot.days_until_show,
        "support_reachable": reachable,
        "shared_with_headliner": shared,
        "adds_reachable": adds,
    });
    // Same rule as the band path: the operator's fix is reviewed against the
    // composed letter, and a refused revision refuses the approval — nothing
    // is written, and the key stays unspent.
    let (payload, applied) = match revision {
        Some(revision) => {
            let (revised, applied) = apply_operator_revision(&payload, revision)?;
            (revised, Some(applied))
        }
        None => (payload, None),
    };
    let opening_line = match &payload {
        AutopilotActionPayload::RequestGigOutreach { opening_line, .. } => opening_line.clone(),
        _ => opening_line,
    };
    let action_id = match queue_support_slot_ask(
        pool,
        headliner_workspace_id,
        slot.event_id,
        &payload,
        applied.as_ref(),
        input_snapshot,
        idempotency_key,
        now,
    )
    .await?
    {
        QueueOutcome::Queued(action_id) => action_id,
        QueueOutcome::Replayed { action_id, status } => {
            return Ok(SupportSlotAskOutcome::Replayed { action_id, status });
        }
    };

    Ok(SupportSlotAskOutcome::Queued {
        action_id,
        headliner,
        support,
        city: slot.city_name,
        venue: slot.venue,
        show_date,
        recipients: recipients.into_iter().map(|target| target.name).collect(),
        opening_line,
    })
}

/// The ask's sentences, composed at approval time from the measured numbers.
///
/// Same rule as the proposal letter: the console and the draft read one set of
/// facts, and neither invents a number the other did not have. `shared` is
/// `None` when the pair was never measured here — the honest version of that
/// is "every one of them is new", not a zero we never counted.
#[allow(clippy::too_many_arguments)]
fn support_slot_ask_letter(
    headliner: &str,
    support: &str,
    city: &str,
    venue: &str,
    show_date: &str,
    reachable: u32,
    shared: Option<u32>,
    adds: u32,
) -> (String, Vec<String>) {
    let opening_line = format!(
        "{support} can take the open slot at {venue} on {show_date} — they bring \
         {adds} people there that we do not already reach."
    );
    let mut reasons = vec![format!(
        "{support} has {reachable} people around {city} who asked to hear when they play"
    )];
    match shared {
        Some(shared) if shared > 0 => reasons.push(format!(
            "{shared} of them already follow {headliner}, so the name still adds {adds} \
             people the night does not reach"
        )),
        Some(_) => reasons.push(format!(
            "none of the people {support} reaches around {city} already follow \
             {headliner} — a second crowd, not the same one twice"
        )),
        None => reasons.push(format!(
            "{headliner} has no measured audience around {city} — every person \
             {support} brings is somebody new to the night"
        )),
    }
    reasons.push(
        "the room and the date are already held — this confirms a name for a slot \
         you offered, it does not ask for a new night"
            .to_owned(),
    );
    (opening_line, reasons)
}

/// Writes the approval and the queued action in one transaction — the same
/// shape as `queue_outreach`, with the show as the subject instead of the
/// city and the roster operator as the approver.
#[allow(clippy::too_many_arguments)]
async fn queue_support_slot_ask(
    pool: &PgPool,
    workspace_id: Uuid,
    event_id: Uuid,
    payload: &AutopilotActionPayload,
    revision: Option<&AppliedRevision>,
    input_snapshot: serde_json::Value,
    idempotency_key: &IdempotencyKey,
    now: OffsetDateTime,
) -> Result<QueueOutcome, GigOutreachError> {
    let payload_json = serde_json::to_value(payload)
        .map_err(|_| GigOutreachError::Refused("the outreach could not be encoded".to_owned()))?;
    let action_kind = payload.action_kind();
    let action_class = payload.action_class().as_str();
    let trace = TraceContext::root(WorkspaceId::from_uuid(workspace_id));
    let decision_key = format!("support-slot-ask:{}", idempotency_key.as_str());

    let mut tx = pool.begin().await?;
    let action_id = Uuid::now_v7();
    // The audit row first, for the same reason as the proposal path: a
    // same-key request that committed since the early replay check is
    // answered from the ledger, not queued twice.
    let operation_id = Uuid::now_v7();
    if record_approval(
        &mut tx,
        workspace_id,
        operation_id,
        "approve_support_slot_ask",
        action_id,
        revision,
        idempotency_key,
    )
    .await?
    {
        tx.rollback().await?;
        return Ok(
            match existing_action(pool, workspace_id, idempotency_key).await? {
                Some((action_id, status)) => QueueOutcome::Replayed { action_id, status },
                None => {
                    return Err(GigOutreachError::Refused(
                        "this idempotency key is already spent on a different action — a new \
                     approval needs a fresh key"
                            .to_owned(),
                    ));
                }
            },
        );
    }
    let decision_id = match sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
        ) VALUES ($1,$2,$3,'booking_opportunity','event',$4,
                  'support_slot.ask.approved',10000,'require_approval',
                  'Roster-approved support-slot ask',
                  $5,$6,$7,$8,$9)
        ON CONFLICT (workspace_id, decision_key) DO NOTHING RETURNING id
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(&decision_key)
    .bind(event_id)
    .bind(&input_snapshot)
    .bind(json!({ "approved_by_roster_operator": true, "one_letter_per_show": true }))
    .bind(&payload_json)
    .bind(now)
    .bind(trace.trace_id().into_uuid())
    .fetch_optional(&mut *tx)
    .await?
    {
        Some(id) => id,
        // A decision under this key exists and the action lookup found none:
        // a prior attempt died between the two inserts. Reuse the decision and
        // queue the action it was meant to carry.
        None => sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM viryaos_autopilot_decisions WHERE workspace_id = $1 AND decision_key = $2",
        )
        .bind(workspace_id)
        .bind(&decision_key)
        .fetch_one(&mut *tx)
        .await?,
    };

    let action_trace = TraceContext::for_action(
        WorkspaceId::from_uuid(workspace_id),
        trace.trace_id(),
        action_id,
        Some(decision_id),
    );
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_actions (
            id, workspace_id, decision_id, context, action_kind,
            subject_kind, subject_id, idempotency_key, payload, status,
            action_class, approved_at, approved_by, available_at,
            trace_id, causation_id
        ) VALUES ($1,$2,$3,'booking_opportunity',$4,'event',$5,$6,$7,
                  'queued',$8,$9,'operator:support_slot_ask_approval',
                  $9 + make_interval(secs => $12::double precision),$10,$11)
        "#,
    )
    .bind(action_id)
    .bind(workspace_id)
    .bind(decision_id)
    .bind(action_kind)
    .bind(event_id)
    .bind(idempotency_key.as_str())
    .bind(&payload_json)
    .bind(action_class)
    .bind(now)
    .bind(action_trace.trace_id().into_uuid())
    .bind(action_trace.causation_id().map(|id| id.into_uuid()))
    // O.2: the ask is a letter to the headliner's promoter — the same outward
    // hold `queue_outreach` writes, so "approve" and "sent" stop being the
    // same instant on this path too.
    .bind(f64::from(
        i32::try_from(payload.action_class().hold_seconds()).unwrap_or(120),
    ))
    .execute(&mut *tx)
    .await?;

    if let Some(revision) = revision {
        record_revision(&mut tx, workspace_id, action_id, operation_id, revision).await?;
    }

    tx.commit().await?;
    Ok(QueueOutcome::Queued(action_id))
}
