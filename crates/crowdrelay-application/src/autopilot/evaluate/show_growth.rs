//! Attendance-growth candidate mapping. The domain decides the lever; this module
//! only turns that decision into a durable, idempotent Autopilot action.

use crowdrelay_domain::show_growth::{
    MAX_LEVER_ATTEMPTS, ShowGrowthDecision, ShowGrowthHoldReason, ShowGrowthLever,
    ShowGrowthPolicy, ShowGrowthSnapshot, evaluate_show_growth_passing_over,
};
use time::OffsetDateTime;

use super::{policy_evidence, *};

/// Failed attempts per `(event id, lever)`, as the cycle read them.
pub(super) type ShowGrowthFailures = std::collections::HashMap<(uuid::Uuid, String), u32>;

/// The crossbill gate's named reason — the token the decision ledger carries
/// when the partner lever is declined (§4e-2).
const UNRECIPROCATED_CROSSBILL: &str = "unreciprocated_crossbill";
/// The decision-ledger token for a lever whose own measured outcomes retired
/// it. Recorded as a Deny so the operator sees the ladder stopped itself,
/// rather than the lever silently never firing again.
const LEVER_RETIRED: &str = "lever_retired";

/// The standing key a lever's measured outcomes accumulate under — the same
/// `action_kind:identity` shape the worker-signals loader groups by.
fn lever_standing_key(lever: ShowGrowthLever) -> String {
    format!("show.growth.request:{}", lever.as_str())
}

pub(super) fn show_growth_candidates(
    snapshot: ShowGrowthSnapshot,
    policy: &AutopilotPolicy,
    evidence: ContextEvidence,
    standings: &std::collections::HashMap<String, Standing>,
    external_executor_live: bool,
    failures: &ShowGrowthFailures,
    now: OffsetDateTime,
) -> Result<Vec<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::ShowGrowth(domain_policy) = policy.config else {
        return Ok(Vec::new());
    };
    let failed = |lever: ShowGrowthLever| {
        failures
            .get(&(snapshot.event_id.into_uuid(), lever.as_str().to_owned()))
            .copied()
            .unwrap_or(0)
    };
    // Passed over: levers only an external executor can carry out while none
    // is live, and levers that failed `MAX_LEVER_ATTEMPTS` times. Either
    // would otherwise hold every later lever of the ladder.
    let evaluate = |snapshot| {
        evaluate_show_growth_passing_over(snapshot, domain_policy, now, |lever| {
            // Relationship selection has one owner. The Beacon evaluator emits
            // RequestBeaconOutreach with a concrete verified beacon_id,
            // suppression state and relationship phase; generic show.growth
            // carries none of those and therefore never owns partner/scene
            // outbound delivery.
            lever.is_beacon_outreach()
                || (!external_executor_live && !lever.is_first_party())
                || failed(lever) >= MAX_LEVER_ATTEMPTS
        })
        .0
    };
    match evaluate(snapshot) {
        ShowGrowthDecision::Request {
            lever,
            confidence,
            send_at,
        } => Ok(vec![request_candidate(
            snapshot,
            policy,
            domain_policy,
            lever,
            confidence,
            evidence,
            standings,
            send_at,
            failed(lever),
        )?]),
        ShowGrowthDecision::Hold(ShowGrowthHoldReason::UnreciprocatedCrossbill) => {
            let mut candidates = vec![unreciprocated_crossbill_decline(
                snapshot,
                policy,
                domain_policy,
            )?];
            // The refusal belongs to the partner lever, not to the ladder —
            // the levers after it stay due on their own schedule. Re-evaluate
            // with the gated lever masked so the same cycle still offers the
            // next one instead of losing every later lever to a held edge.
            let mut masked = snapshot;
            masked.history.partner_cross_promo_requested = true;
            if let ShowGrowthDecision::Request {
                lever,
                confidence,
                send_at,
            } = evaluate(masked)
            {
                candidates.push(request_candidate(
                    snapshot,
                    policy,
                    domain_policy,
                    lever,
                    confidence,
                    evidence,
                    standings,
                    send_at,
                    failed(lever),
                )?);
            }
            Ok(candidates)
        }
        ShowGrowthDecision::Hold(_) => Ok(Vec::new()),
    }
}

#[allow(clippy::too_many_arguments)]
fn request_candidate(
    snapshot: ShowGrowthSnapshot,
    policy: &AutopilotPolicy,
    domain_policy: ShowGrowthPolicy,
    lever: ShowGrowthLever,
    confidence: Confidence,
    evidence: ContextEvidence,
    standings: &std::collections::HashMap<String, Standing>,
    send_at: Option<OffsetDateTime>,
    failed_attempts: u32,
) -> Result<DecisionCandidate, serde_json::Error> {
    // Standing is the lever's own measured record — a run of worsened
    // outcomes retires it the same way it retires a worker template. An
    // absent key means untested: full reach, because an unmeasured lever has
    // earned neither trust nor refusal.
    if standings
        .get(lever_standing_key(lever).as_str())
        .is_some_and(|standing| standing.is_retired())
    {
        return lever_retired_decline(snapshot, policy, domain_policy, lever);
    }
    let disposition = disposition_with_evidence(
        policy.autonomy_level,
        confidence,
        policy.minimum_confidence,
        evidence,
        RATE_FLOOR,
    );
    let mut policy_snapshot = policy_evidence(policy, domain_policy)?;
    // A broad show-ladder approval may release repeatable owned/first-party
    // promotion. Relationship-sensitive moves stay per-action regardless of
    // booking cadence: silence in the interaction ledger is not authority.
    if snapshot.ladder_approved
        && lever.ladder_may_pre_authorize(snapshot.human_booking_targets_30d)
        && let Some(map) = policy_snapshot.as_object_mut()
    {
        map.insert(
            "ladder_authorized".to_owned(),
            serde_json::Value::Bool(true),
        );

    }
    let action = AutopilotActionPayload::RequestShowGrowth {
        event_id: snapshot.event_id,
        lever,
        template_key: lever.template_key().to_owned(),
        send_at,
    };
    Ok(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::Event(snapshot.event_id),
        decision_kind: "activate_show_growth_lever",
        confidence,
        disposition,
        reason: "a bounded attendance-growth lever is due from first-party show evidence",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot,
        action,
        decision_key: format!(
            "decision:show-growth:v{}:{}:{}:{}:{}:{}:{}:{}:{}",
            policy.version,
            snapshot.event_id,
            lever.as_str(),
            snapshot.paid_tickets,
            snapshot.paid_tickets_last_7d,
            snapshot.interested_fans,
            snapshot.city_signal_fans,
            snapshot.beacon_partners,
            snapshot.human_booking_targets_30d,
        ),
        // Each lever is one-shot per event: a later wave should be a distinct
        // lever, not a repeat. A failed attempt is the exception — it
        // happened to nobody — so its retry carries its attempt number and
        // gets a key of its own, up to `MAX_LEVER_ATTEMPTS`.
        action_idempotency_key: if failed_attempts == 0 {
            format!(
                "action:show-growth:{}:{}",
                snapshot.event_id,
                lever.as_str()
            )
        } else {
            format!(
                "action:show-growth:{}:{}:retry{failed_attempts}",
                snapshot.event_id,
                lever.as_str()
            )
        },
    })
}

/// §4e-2: the deterministic refusal of the partner lever on an unreciprocated
/// crossbill edge. A `Deny` writes the decision row — the named reason, the
/// snapshot that measured it and the lever that was declined — without an
/// action row, the same ledger channel the §4i-2 show-week hold uses. The key
/// is stable so the refusal is recorded once; when a reverse delivery lands
/// the ordinary request key fires the lever as if it had never been held.
fn unreciprocated_crossbill_decline(
    snapshot: ShowGrowthSnapshot,
    policy: &AutopilotPolicy,
    domain_policy: ShowGrowthPolicy,
) -> Result<DecisionCandidate, serde_json::Error> {
    Ok(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::Event(snapshot.event_id),
        decision_kind: UNRECIPROCATED_CROSSBILL,
        confidence: Confidence::saturating_from_basis_points(9_400),
        disposition: PolicyDisposition::Deny,
        reason: "unreciprocated_crossbill: this workspace's audience has never carried the \
                 partner's announcement — carrying theirs first is how the edge unlocks",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::RequestShowGrowth {
            event_id: snapshot.event_id,
            lever: ShowGrowthLever::PartnerCrossPromo,
            template_key: ShowGrowthLever::PartnerCrossPromo.template_key().to_owned(),
            send_at: None,
        },
        decision_key: format!(
            "decision:show-growth:v{}:{}:{UNRECIPROCATED_CROSSBILL}",
            policy.version, snapshot.event_id,
        ),
        action_idempotency_key: format!(
            "action:show-growth:{}:partner_cross_promo:{UNRECIPROCATED_CROSSBILL}",
            snapshot.event_id,
        ),
    })
}

/// A lever whose measured record retired it is refused on the same ledger
/// channel as the crossbill hold: a `Deny` decision naming the lever and the
/// reason, with no action row. Standing is workspace-global, not per-event,
/// so the refusal keys on the lever alone — the same verdict would be
/// written for every event, and one row says it. When outcomes stop being
/// worsened the standing recovers on its own and the ordinary request key
/// fires the lever as if it had never been held.
fn lever_retired_decline(
    snapshot: ShowGrowthSnapshot,
    policy: &AutopilotPolicy,
    domain_policy: ShowGrowthPolicy,
    lever: ShowGrowthLever,
) -> Result<DecisionCandidate, serde_json::Error> {
    Ok(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::Event(snapshot.event_id),
        decision_kind: LEVER_RETIRED,
        confidence: Confidence::saturating_from_basis_points(9_400),
        disposition: PolicyDisposition::Deny,
        reason: "lever_retired: this lever's own measured outcomes kept worsening, \
                 so the brain stopped proposing it until an operator reinstates it",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::RequestShowGrowth {
            event_id: snapshot.event_id,
            lever,
            template_key: lever.template_key().to_owned(),
            send_at: None,
        },
        decision_key: format!(
            "decision:show-growth:v{}:{LEVER_RETIRED}:{}",
            policy.version,
            lever.as_str(),
        ),
        action_idempotency_key: format!("action:show-growth:{LEVER_RETIRED}:{}", lever.as_str(),),
    })
}
