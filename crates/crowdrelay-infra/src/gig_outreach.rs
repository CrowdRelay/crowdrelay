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
//! # Approving with a fix (N.10)
//!
//! The letter is the outreach the band cares most about getting right, so the
//! generic approve-with-revision machinery (2.9) reaches it here: the caller
//! may carry `{"revision": {"opening_line": "..."}}` and the approval is
//! reviewed, applied and recorded before anything is queued. A refused
//! revision refuses the whole approval rather than approving the original
//! words — and writes nothing, so the idempotency key stays unspent and the
//! band can fix the edit and click again. An accepted revision lands in the
//! payload the action carries, and `viryaos_draft_revisions` keeps the
//! before/after the voice signal (§4d-3.2) is measured from.
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

use std::collections::BTreeMap;

use crowdrelay_application::autopilot::{
    AutopilotActionPayload, GigLetterKind, GigOutreachRecipient,
};
use crowdrelay_application::{IdempotencyKey, RepositoryError};
use crowdrelay_domain::gig_plan::{GigPlan, plan_gig};
use crowdrelay_domain::trace::TraceContext;
use crowdrelay_domain::{BookingTargetId, CityId, WorkspaceId};
use serde_json::json;
use sqlx::PgPool;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::gig_planning::{city_opportunities, promoter_targets_in_city, stated_intent};
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

/// The edit an operator made to the letter, carried from the review into the
/// queue transaction.
///
/// `requested` is every field the operator asked to touch — the audit row's
/// record of intent. `changed` is what survived review, and `before` is the
/// draft as the machine wrote it; the two together are what each
/// `viryaos_draft_revisions` row records.
struct AppliedRevision {
    requested: Vec<String>,
    changed: BTreeMap<String, String>,
    before: BTreeMap<String, String>,
}

/// What a queue transaction settled into.
///
/// `Replayed` is the mid-transaction version of the replay check at the top
/// of each approve: a same-key request committed between that check and this
/// write, so its action is the answer — the same one the check would have
/// given a moment later.
enum QueueOutcome {
    Queued(Uuid),
    Replayed { action_id: Uuid, status: String },
}

/// Reviews the operator's edit against the letter as composed and returns the
/// payload with it applied.
///
/// Runs before the queue transaction opens rather than inside it: the payload
/// was composed in this call, so there is no stored draft to lock against —
/// and a refused revision must leave nothing behind, including a spent
/// idempotency key. The refusal is the review's own sentence, shown to the
/// band rather than translated.
fn apply_operator_revision(
    payload: &AutopilotActionPayload,
    revision: &BTreeMap<String, String>,
) -> Result<(AutopilotActionPayload, AppliedRevision), GigOutreachError> {
    let mut serialized = serde_json::to_value(payload)
        .map_err(|_| GigOutreachError::Refused("the outreach could not be encoded".to_owned()))?;
    let draft = crowdrelay_domain::draft_revision::revisable_fields(&serialized);
    let changed = crowdrelay_domain::draft_revision::review_revision(&draft, revision)
        .map_err(|refusal| GigOutreachError::Refused(refusal.message()))?;
    crowdrelay_domain::draft_revision::apply_revision(&mut serialized, &changed);
    let revised = serde_json::from_value(serialized)
        .map_err(|_| GigOutreachError::Refused("the outreach could not be encoded".to_owned()))?;
    Ok((
        revised,
        AppliedRevision {
            requested: revision.keys().cloned().collect(),
            changed,
            before: draft,
        },
    ))
}

/// The approval's audit row, written before the rows it approves.
///
/// Every gig approval is an operator action worth recording — revised or not —
/// keyed on the caller's idempotency key so a same-key request that committed
/// between the early replay check and this write is answered from the ledger
/// instead of queuing a second letter. `true` means the key was already spent
/// and the caller should answer from the action ledger.
async fn record_approval(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: Uuid,
    operation_id: Uuid,
    operator_action: &'static str,
    action_id: Uuid,
    revision: Option<&AppliedRevision>,
    idempotency_key: &IdempotencyKey,
) -> Result<bool, GigOutreachError> {
    let details = match revision {
        Some(revision) => json!({ "revision_fields": revision.requested }),
        None => json!({}),
    };
    match crate::autopilot::operator_actions::insert_operator_action(
        tx,
        WorkspaceId::from_uuid(workspace_id),
        operation_id,
        operator_action,
        "autopilot_action",
        action_id,
        "admin_api_key",
        idempotency_key,
        None,
        &details,
    )
    .await
    {
        Ok(None) => Ok(false),
        Ok(Some(_)) | Err(RepositoryError::Conflict) => Ok(true),
        // The helper maps every database failure to a RepositoryError with no
        // payload; the honest translation left is the one the sendability
        // read above already makes — an infrastructure failure, not a
        // refusal the band could act on.
        Err(_) => Err(GigOutreachError::Database(sqlx::Error::Protocol(
            "operator action ledger write failed".to_owned(),
        ))),
    }
}

/// §4d-3.1 — every field the operator fixed gets its own ledger row: the
/// machine's words, the band's words, and how far they moved. The rows hang
/// off the approval's `operator_actions` id — the edit is what that approval
/// did — and the per-field distance uses the same rule the trend reads.
async fn record_revision(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: Uuid,
    action_id: Uuid,
    operation_id: Uuid,
    revision: &AppliedRevision,
) -> Result<(), GigOutreachError> {
    for (field, after) in &revision.changed {
        let before = revision.before.get(field).cloned().unwrap_or_default();
        let mut single = BTreeMap::new();
        single.insert(field.clone(), after.clone());
        let distance =
            crowdrelay_domain::draft_revision::revision_distance(&revision.before, &single);
        sqlx::query(
            r#"
            INSERT INTO viryaos_draft_revisions
                (workspace_id, action_id, operation_id, field,
                 before_text, after_text, distance_chars)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(workspace_id)
        .bind(action_id)
        .bind(operation_id)
        .bind(field)
        .bind(before)
        .bind(after)
        .bind(i64::try_from(distance).unwrap_or(i64::MAX))
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
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
/// `revision` is the approve-with-edit path (N.10): the band's fix to the
/// letter's words, reviewed against the composed payload before anything is
/// written. Only `opening_line` is revisable — the reasons are the machine's
/// evidence, and a disagreement with a number is a refusal, not an edit.
///
/// # Errors
///
/// `NotFound` when the city is not among this workspace's opportunities.
/// `Refused` when the planner no longer proposes it, when nobody it names
/// resolves to a contactable promoter, or when the revision is refused.
/// Database errors propagate.
pub async fn approve_gig_proposal(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    idempotency_key: &IdempotencyKey,
    now: OffsetDateTime,
    revision: Option<&BTreeMap<String, String>>,
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
    // The band's fix to the letter, reviewed before anything is queued: a
    // refused revision refuses the approval itself, and nothing — no
    // decision, no action, no ledger row — is written for it.
    let (payload, applied) = match revision {
        Some(revision) => {
            let (revised, applied) = apply_operator_revision(&payload, revision)?;
            (revised, Some(applied))
        }
        None => (payload, None),
    };
    // The outcome reports the line the promoter will actually read — the
    // operator's words when a revision replaced the machine's.
    let opening_line = match &payload {
        AutopilotActionPayload::RequestGigOutreach { opening_line, .. } => opening_line.clone(),
        _ => plan.opening_line(),
    };
    let action_id = match queue_outreach(
        pool,
        workspace_id,
        &plan,
        city_id,
        &payload,
        applied.as_ref(),
        idempotency_key,
        now,
    )
    .await?
    {
        QueueOutcome::Queued(action_id) => action_id,
        QueueOutcome::Replayed { action_id, status } => {
            return Ok(GigOutreachOutcome::Replayed { action_id, status });
        }
    };

    Ok(GigOutreachOutcome::Queued {
        action_id,
        city: plan.city.clone(),
        venue: plan.venue.clone(),
        recipients: recipients
            .into_iter()
            .map(|recipient| recipient.2)
            .collect(),
        opening_line,
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

/// Writes the approval, the decision and the queued action in one
/// transaction.
///
/// The decision exists so the outreach has the same provenance as anything the
/// brain proposes: `ops/trace/{trace_id}` joins decision, action, outbox and
/// delivery, and an action with no decision is a send nobody can explain. The
/// band's approval is the decision — recorded as `require_approval` with the
/// approval already granted, because that is what happened. The approval
/// itself is an operator action, and an operator's edit leaves a
/// `viryaos_draft_revisions` row per field — all of it in this transaction,
/// so the letter and the record of approving it exist together or not at all.
#[allow(clippy::too_many_arguments)]
async fn queue_outreach(
    pool: &PgPool,
    workspace_id: Uuid,
    plan: &GigPlan,
    city_id: Uuid,
    payload: &AutopilotActionPayload,
    revision: Option<&AppliedRevision>,
    idempotency_key: &IdempotencyKey,
    now: OffsetDateTime,
) -> Result<QueueOutcome, GigOutreachError> {
    let payload_json = serde_json::to_value(payload)
        .map_err(|_| GigOutreachError::Refused("the outreach could not be encoded".to_owned()))?;
    let action_kind = payload.action_kind();
    let action_class = payload.action_class().as_str();
    let trace = TraceContext::root(WorkspaceId::from_uuid(workspace_id));
    let decision_key = format!("gig.outreach:{}", idempotency_key.as_str());

    let mut tx = pool.begin().await?;
    let action_id = Uuid::now_v7();
    // The audit row goes first — it is the key's claim on this request. A
    // same-key request that committed between the early replay check and now
    // turns up here, and its action is the answer rather than a second
    // letter.
    let operation_id = Uuid::now_v7();
    if record_approval(
        &mut tx,
        workspace_id,
        operation_id,
        "approve_gig_proposal",
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
                // The key is spent on an operator action that is not this letter
                // — a key reused across controls is a caller bug, and the honest
                // answer is a sentence rather than a replay of somebody else's
                // approval.
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

    if let Some(revision) = revision {
        record_revision(&mut tx, workspace_id, action_id, operation_id, revision).await?;
    }

    tx.commit().await?;
    Ok(QueueOutcome::Queued(action_id))
}

// ── N.5: the support-slot ask ──────────────────────────────────────────────
//
// The same machinery answers a second approval, made by the roster operator
// rather than the band — it lives in `support_slot` now, and this file keeps
// the pieces both approvals share.
mod support_slot;
pub use support_slot::{SupportSlotAskOutcome, approve_support_slot_ask};
