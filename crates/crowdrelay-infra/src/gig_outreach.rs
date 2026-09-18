//! Approving a gig proposal, and the outreach that approval produces (4G.4).
//!
//! # The proposal is recomputed, never replayed from the screen
//!
//! The band approves a city, not a document. This re-runs `plan_gig` over the
//! evidence as it stands right now and writes the outreach from *that* — so a
//! proposal that stopped being true between the read and the click refuses
//! instead of sending. The room went dormant, the band booked the city, the
//! promoter was marked do-not-contact: each of those is a refusal here, in the
//! same sentence the console would have shown.
//!
//! It is also why nothing has to be stored between the two. `plan_gig` is
//! deterministic over its input, which is asserted in the domain tests, so the
//! proposal the band read is the proposal this writes unless the evidence moved
//! — and if it moved, the evidence wins.
//!
//! # One action, every promoter
//!
//! §12-6: the approval produces **one** action carrying a recipient set, not
//! one action per promoter. Separate actions reserve contact windows
//! separately, so the governor can admit the first and refuse the second, and
//! the band has then written to one of three promoters about one night. Those
//! three people book the same city and talk to each other. One action reserves
//! all of them in a single transaction — everybody hears, or nobody does and
//! the band is told which promoter is blocked and why.
//!
//! # Approving once is approving
//!
//! The action is written `queued`, not `awaiting_approval`. The band has just
//! read the reasons, the room, the caveats and the names, and said yes; asking
//! them to approve the same thing again on a different screen is how a person
//! stops reading approvals. The execution arm still re-runs every gate at
//! dispatch — the row lock, the contact governor, the target's own flags — so
//! nothing here is trusted past the moment it was checked.
//!
//! # The support-slot ask (N.5)
//!
//! The same machinery answers a second approval, made by the roster operator
//! rather than the band: an open slot on a confirmed show is read, a labelmate
//! is named, and the letter goes to the headliner's *own* promoter — the act
//! that already holds the room. The differences are deliberate ones. The
//! action lives on the headliner's workspace because the relationship and the
//! night are theirs. The subject is the event, not the city — the ask is
//! about one show's bill, and a second ask while the first is unanswered is
//! the same letter. And every gate recomputes from the rows: the slot, the
//! pairing arithmetic, the sender, the names — nothing arrives trusted from
//! the screen.

use crowdrelay_application::IdempotencyKey;
use crowdrelay_application::autopilot::{
    AutopilotActionPayload, GigLetterKind, GigOutreachRecipient,
};
use crowdrelay_domain::gig_plan::{GigPlan, plan_gig};
use crowdrelay_domain::roster_plan::{
    MINIMUM_SLOT_LEAD_DAYS, PAIRING_OVERLAP_CEILING_BASIS_POINTS,
};
use crowdrelay_domain::trace::TraceContext;
use crowdrelay_domain::{BookingTargetId, CityId, EventId, WorkspaceId};
use serde_json::json;
use sqlx::PgPool;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::gig_planning::{city_opportunities, promoter_targets_in_city, stated_intent};
use crate::place_reach::{audience_overlaps_by_city, reachable_in_city};
use crate::tenant_settings::TenantSettingsRepository;

#[derive(Debug, Error)]
pub enum GigOutreachError {
    #[error("gig outreach database operation failed")]
    Database(#[from] sqlx::Error),
    /// No such city in this workspace's opportunities. Not a refusal: the band
    /// asked about something that is not on the board.
    #[error("no such city")]
    NotFound,
    /// The evidence no longer supports the proposal, or there is nobody to
    /// write to. Carries the band-facing sentence because the caller's job is
    /// to show it, not to translate it.
    #[error("{0}")]
    Refused(String),
}

/// What `approve_gig_proposal` did.
///
/// `Replayed` means this idempotency key already produced an outreach — the
/// caller gets the existing action and its real stored status, which may
/// already be `succeeded` or `failed`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GigOutreachOutcome {
    Queued {
        action_id: Uuid,
        city: String,
        venue: String,
        recipients: Vec<String>,
        opening_line: String,
    },
    Replayed {
        action_id: Uuid,
        status: String,
    },
}

/// The capability an executor must advertise before a gig letter can leave.
///
/// Its own capability rather than `booking.outreach`: this event carries a
/// recipient set and a different template, so an executor built for the
/// single-promoter letter would claim it and have nothing to do with it.
pub const GIG_OUTREACH_CAPABILITY: &str = "gig.outreach";

/// What the band is told when no executor can send the letter.
///
/// Named rather than phrased at each call site, because the plan read and the
/// approval must say the same thing: a console that offers a button its own
/// backend will refuse is worse than one that greys it out.
pub const SEND_CHANNEL_MISSING: &str = "nothing can send this letter yet — no connected sender advertises gig outreach, so an \
     approval would be queued, parked and cancelled a day later without anybody hearing from \
     you. Connect the sender first, and this proposal is still here.";

/// Whether a gig letter approved right now could actually be sent.
///
/// # Errors
///
/// Propagates the database error.
pub async fn gig_outreach_is_sendable(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<bool, GigOutreachError> {
    crate::autopilot::capability_is_serviceable(
        pool,
        WorkspaceId::from_uuid(workspace_id),
        GIG_OUTREACH_CAPABILITY,
    )
    .await
    .map_err(|_| {
        GigOutreachError::Database(sqlx::Error::Protocol(
            "executor registry read failed".to_owned(),
        ))
    })
}

/// Approves the proposal for one city and queues the outreach it names.
///
/// # Errors
///
/// `NotFound` when the city is not among this workspace's opportunities.
/// `Refused` when the planner no longer proposes it, or when nobody it names
/// resolves to a contactable promoter. Database errors propagate.
pub async fn approve_gig_proposal(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    idempotency_key: &IdempotencyKey,
    now: OffsetDateTime,
) -> Result<GigOutreachOutcome, GigOutreachError> {
    // A replay answers from the ledger before any evidence is read. The second
    // click of a button that already worked must not depend on the city still
    // being proposable.
    if let Some((action_id, status)) = existing_action(pool, workspace_id, idempotency_key).await? {
        return Ok(GigOutreachOutcome::Replayed { action_id, status });
    }

    // Asked before any evidence is read, because the answer does not depend on
    // the evidence: if nothing can send this letter, approving it produces a
    // queued action that parks and is cancelled a day later by a sweep nobody
    // is watching. The band would have said yes to a send that never happened.
    if !gig_outreach_is_sendable(pool, workspace_id).await? {
        return Err(GigOutreachError::Refused(SEND_CHANNEL_MISSING.to_owned()));
    }

    let settings = TenantSettingsRepository::new(pool.clone());
    let intent = stated_intent(&settings, workspace_id).await?;
    let opportunities = city_opportunities(pool, workspace_id, now).await?;
    // Matched on the catalogue id, not the slug — two cities in different
    // countries can share one slug, and the proposal a band approved belongs
    // to exactly one of them.
    let opportunity = opportunities
        .iter()
        .find(|candidate| candidate.city_id == CityId::from_uuid(city_id))
        .ok_or(GigOutreachError::NotFound)?;

    let plan = plan_gig(opportunity, intent)
        .map_err(|refusal| GigOutreachError::Refused(refusal.message()))?;

    // A letter to this city that has not finished yet is the same letter. The
    // action ledger's in-flight index refuses the second write anyway, and a
    // unique-violation reaching the band as a 503 says the system is broken
    // when the truth is that they already said yes. Asked after the evidence,
    // because a city that stopped being proposable has a better answer than
    // this one: what changed.
    if let Some(status) = inflight_status(pool, workspace_id, city_id).await? {
        return Err(GigOutreachError::Refused(format!(
            "you have already approved this city — the letter is {status}. Cancel it on the \
             operations board if you want to write a different one."
        )));
    }

    let recipients = recipients_for(pool, workspace_id, city_id, &plan).await?;
    if recipients.is_empty() {
        return Err(GigOutreachError::Refused(format!(
            "nobody who books in {city} is contactable any more. The proposal named \
             {named}, and none of them is still an active booking contact — check the \
             booking list before writing",
            city = opportunity.city,
            named = plan
                .contact
                .iter()
                .map(|contact| contact.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    let payload = AutopilotActionPayload::RequestGigOutreach {
        city_id: CityId::from_uuid(city_id),
        venue: plan.venue.clone(),
        recipients: recipients
            .iter()
            .map(|recipient| GigOutreachRecipient {
                target_id: BookingTargetId::from_uuid(recipient.0),
                target_version: recipient.1,
                target_name: recipient.2.clone(),
            })
            .collect(),
        opening_line: plan.opening_line(),
        // Rendered here rather than in the draft, from the same reasons the
        // console displayed. A draft that re-derives them is a second code path
        // describing one decision.
        reasons: plan.reasons.iter().map(reason_sentence).collect(),
        letter: GigLetterKind::Proposal,
    };
    let action_id = queue_outreach(
        pool,
        workspace_id,
        &plan,
        city_id,
        &payload,
        idempotency_key,
        now,
    )
    .await?;

    Ok(GigOutreachOutcome::Queued {
        action_id,
        city: plan.city.clone(),
        venue: plan.venue.clone(),
        recipients: recipients
            .into_iter()
            .map(|recipient| recipient.2)
            .collect(),
        opening_line: plan.opening_line(),
    })
}

/// One reason as a sentence the draft can use.
///
/// The console phrases these for a band; this phrases them for the promoter who
/// receives the letter, from the same structured fact. Neither one invents a
/// number the other did not have.
fn reason_sentence(reason: &crowdrelay_domain::gig_plan::Reason) -> String {
    use crowdrelay_domain::gig_plan::Reason;
    match reason {
        Reason::ComparableActsPlayedHere { count, .. } => {
            // All-time count over a twelve-month window — the subset phrasing
            // is not guaranteed by the number, so the sentence states the
            // record.
            if *count == 1 {
                "one act from our genre has played there on record".to_owned()
            } else {
                format!("{count} acts from our genre have played there on record")
            }
        }
        Reason::ReachableAudience { reachable } => {
            format!("{reachable} people nearby asked us to tell them when we play")
        }
        Reason::RoomDraws { typical_draw } => {
            format!("the room averages {typical_draw} paid tickets per ticketed show")
        }
        Reason::NeverPlayedButHasFans { reachable } => {
            format!("{reachable} people nearby follow us and we have never played the city")
        }
        Reason::OverdueReturn { months, active_30d } => format!(
            "our last show there was {months} months ago and {active_30d} people there were \
             active with us this month"
        ),
        Reason::CoBillAddsAudience {
            act,
            adds_reachable,
        } => format!("a bill with {act} reaches {adds_reachable} people we do not reach alone"),
        Reason::WarmPromoter { name } => format!("{name} has answered us before"),
        Reason::RoomIsActive {
            days_since_last_event,
        } => format!("the room had something on {days_since_last_event} days ago"),
    }
}

/// The promoters the proposal named, in the order it ranked them.
///
/// Resolved by the key the proposal carried — the booking target's own id —
/// rather than by display name. Two people who book one city can both be
/// "Anna", the booking list is unique on the address rather than the name, and
/// name matching silently addressed one of them twice while never writing to
/// the other. A key the read no longer returns is dropped rather than guessed
/// at, and the caller refuses when nothing survives.
async fn recipients_for(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    plan: &GigPlan,
) -> Result<Vec<(Uuid, i64, String)>, GigOutreachError> {
    let targets = promoter_targets_in_city(pool, workspace_id, city_id).await?;
    Ok(plan
        .contact
        .iter()
        .filter_map(|contact| {
            targets
                .iter()
                .find(|target| target.target_id.to_string() == contact.key)
        })
        .map(|target| (target.target_id, target.target_version, target.name.clone()))
        .collect())
}

/// The status of a gig letter for this subject that has not finished yet, if
/// one exists.
///
/// Mirrors `viryaos_autopilot_actions_inflight_subject_uidx` — the partial
/// unique index on `(workspace_id, context, action_kind, subject_id)` over the
/// unfinished states. Reading it rather than letting the insert collide keeps
/// the answer a sentence the band can act on. The subject is whatever the
/// letter is about — a city for the proposal, a show for the support-slot ask
/// — and the index does not know the difference, which is the point.
async fn inflight_status(
    pool: &PgPool,
    workspace_id: Uuid,
    subject_id: Uuid,
) -> Result<Option<String>, GigOutreachError> {
    Ok(sqlx::query_scalar::<_, String>(
        r#"
        SELECT status FROM viryaos_autopilot_actions
        WHERE workspace_id = $1
          AND context = 'booking_opportunity'
          AND action_kind = 'gig.outreach.request'
          AND subject_id = $2
          AND status IN ('awaiting_approval', 'queued', 'processing')
        ORDER BY created_at DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(subject_id)
    .fetch_optional(pool)
    .await?)
}

async fn existing_action(
    pool: &PgPool,
    workspace_id: Uuid,
    idempotency_key: &IdempotencyKey,
) -> Result<Option<(Uuid, String)>, GigOutreachError> {
    Ok(sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT id, status FROM viryaos_autopilot_actions
        WHERE workspace_id = $1 AND idempotency_key = $2
        "#,
    )
    .bind(workspace_id)
    .bind(idempotency_key.as_str())
    .fetch_optional(pool)
    .await?)
}

/// Writes the decision and the queued action in one transaction.
///
/// The decision exists so the outreach has the same provenance as anything the
/// brain proposes: `ops/trace/{trace_id}` joins decision, action, outbox and
/// delivery, and an action with no decision is a send nobody can explain. The
/// band's approval is the decision — recorded as `require_approval` with the
/// approval already granted, because that is what happened.
async fn queue_outreach(
    pool: &PgPool,
    workspace_id: Uuid,
    plan: &GigPlan,
    city_id: Uuid,
    payload: &AutopilotActionPayload,
    idempotency_key: &IdempotencyKey,
    now: OffsetDateTime,
) -> Result<Uuid, GigOutreachError> {
    let payload_json = serde_json::to_value(payload)
        .map_err(|_| GigOutreachError::Refused("the outreach could not be encoded".to_owned()))?;
    let action_kind = payload.action_kind();
    let action_class = payload.action_class().as_str();
    let trace = TraceContext::root(WorkspaceId::from_uuid(workspace_id));
    let decision_key = format!("gig.outreach:{}", idempotency_key.as_str());

    let mut tx = pool.begin().await?;
    let decision_id = match sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
        ) VALUES ($1,$2,$3,'booking_opportunity','city',$4,
                  'gig.proposal.approved',10000,'require_approval',
                  'Band-approved gig proposal',
                  $5,$6,$7,$8,$9)
        ON CONFLICT (workspace_id, decision_key) DO NOTHING RETURNING id
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(&decision_key)
    .bind(city_id)
    .bind(json!({
        "city": plan.city,
        "venue": plan.venue,
        "contact": plan.contact,
        "reasons": plan.reasons,
        "reach": plan.reach,
        "caveats": plan.caveats,
    }))
    .bind(json!({ "approved_by_band": true, "one_action_per_room": true }))
    .bind(&payload_json)
    .bind(now)
    .bind(trace.trace_id().into_uuid())
    .fetch_optional(&mut *tx)
    .await?
    {
        Some(id) => id,
        // A decision under this key exists and the action lookup found none, so
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

    let action_id = Uuid::now_v7();
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
        ) VALUES ($1,$2,$3,'booking_opportunity',$4,'city',$5,$6,$7,
                  'queued',$8,$9,'operator:gig_proposal_approval',$9,$10,$11)
        "#,
    )
    .bind(action_id)
    .bind(workspace_id)
    .bind(decision_id)
    .bind(action_kind)
    .bind(city_id)
    .bind(idempotency_key.as_str())
    .bind(&payload_json)
    .bind(action_class)
    .bind(now)
    .bind(action_trace.trace_id().into_uuid())
    .bind(action_trace.causation_id().map(|id| id.into_uuid()))
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(action_id)
}

// ── N.5: the support-slot ask ──────────────────────────────────────────────
//
// The cheapest gig in the system is a confirmed show with room on the bill —
// `open_support_slots` reads them, `plan_roster_run` ranks filling one above
// booking a new night, and until now the chain stopped at the plan. This is
// the write half: the roster operator names the headliner and the labelmate,
// and the headliner's own promoter gets one letter asking to confirm the
// support act for the slot they offered.
//
// The differences from `approve_gig_proposal` are the honest ones. The ask
// lives on the headliner's workspace — the room, the promoter relationship
// and the night are theirs, and a letter from the label's account would be a
// stranger writing to somebody else's booker. There is no proposal to replay:
// the plan named an act, and the gates below recompute whether that act still
// fills this slot rather than re-asking the planner who it would pick — the
// operator's choice is the input, the evidence is the constraint.

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
        /// The labelmate being put forward for it.
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
/// # Errors
///
/// `NotFound` when either workspace is not on this roster — the ask names
/// something that is not on the board. `Refused` when the slot is gone, the
/// support fills nothing, the pairing splits one crowd, the headliner cannot
/// send, nobody books the city, or an ask for this show is already in flight.
/// Database errors propagate.
#[allow(clippy::too_many_arguments)]
pub async fn approve_support_slot_ask(
    pool: &PgPool,
    organization_id: Uuid,
    headliner_workspace_id: Uuid,
    support_workspace_id: Uuid,
    city_id: Uuid,
    idempotency_key: &IdempotencyKey,
    now: OffsetDateTime,
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
        reasons,
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
    let action_id = queue_support_slot_ask(
        pool,
        headliner_workspace_id,
        slot.event_id,
        &payload,
        input_snapshot,
        idempotency_key,
        now,
    )
    .await?;

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
async fn queue_support_slot_ask(
    pool: &PgPool,
    workspace_id: Uuid,
    event_id: Uuid,
    payload: &AutopilotActionPayload,
    input_snapshot: serde_json::Value,
    idempotency_key: &IdempotencyKey,
    now: OffsetDateTime,
) -> Result<Uuid, GigOutreachError> {
    let payload_json = serde_json::to_value(payload)
        .map_err(|_| GigOutreachError::Refused("the outreach could not be encoded".to_owned()))?;
    let action_kind = payload.action_kind();
    let action_class = payload.action_class().as_str();
    let trace = TraceContext::root(WorkspaceId::from_uuid(workspace_id));
    let decision_key = format!("support-slot-ask:{}", idempotency_key.as_str());

    let mut tx = pool.begin().await?;
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

    let action_id = Uuid::now_v7();
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
                  'queued',$8,$9,'operator:support_slot_ask_approval',$9,$10,$11)
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
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(action_id)
}
