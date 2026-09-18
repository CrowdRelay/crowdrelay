//! Attendance-growth candidate mapping. The domain decides the lever; this module
//! only turns that decision into a durable, idempotent Autopilot action.

use crowdrelay_domain::show_growth::{
    ShowGrowthDecision, ShowGrowthHoldReason, ShowGrowthLever, ShowGrowthPolicy,
    ShowGrowthSnapshot, evaluate_show_growth,
};
use time::OffsetDateTime;

use super::{policy_evidence, *};

/// The crossbill gate's named reason — the token the decision ledger carries
/// when the partner lever is declined (§4e-2).
const UNRECIPROCATED_CROSSBILL: &str = "unreciprocated_crossbill";

pub(super) fn show_growth_candidates(
    snapshot: ShowGrowthSnapshot,
    policy: &AutopilotPolicy,
    evidence: EvidenceCount,
    now: OffsetDateTime,
) -> Result<Vec<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::ShowGrowth(domain_policy) = policy.config else {
        return Ok(Vec::new());
    };
    match evaluate_show_growth(snapshot, domain_policy, now) {
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
            send_at,
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
            } = evaluate_show_growth(masked, domain_policy, now)
            {
                candidates.push(request_candidate(
                    snapshot,
                    policy,
                    domain_policy,
                    lever,
                    confidence,
                    evidence,
                    send_at,
                )?);
            }
            Ok(candidates)
        }
        ShowGrowthDecision::Hold(_) => Ok(Vec::new()),
    }
}

fn request_candidate(
    snapshot: ShowGrowthSnapshot,
    policy: &AutopilotPolicy,
    domain_policy: ShowGrowthPolicy,
    lever: ShowGrowthLever,
    confidence: Confidence,
    evidence: EvidenceCount,
    send_at: Option<OffsetDateTime>,
) -> Result<DecisionCandidate, serde_json::Error> {
    let disposition = disposition_with_evidence(
        policy.autonomy_level,
        confidence,
        policy.minimum_confidence,
        evidence,
        RATE_FLOOR,
    );
    let mut policy_snapshot = policy_evidence(policy, domain_policy)?;
    // P.4: a live ladder approval is the operator's one "yes" over the whole
    // announce-to-recap sequence, carried as provenance rather than a
    // disposition override — the class ceiling and the envelope still get
    // their say first, and `Deny` is never lifted: approving the ladder was
    // never approving a lever the night's own facts cannot carry. The action
    // insert reads the flag and records `operator:show_ladder` as the rung's
    // approver, so a later revoke cancels exactly what the ladder released.
    if snapshot.ladder_approved
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
            "decision:show-growth:v{}:{}:{}:{}:{}:{}:{}:{}",
            policy.version,
            snapshot.event_id,
            lever.as_str(),
            snapshot.paid_tickets,
            snapshot.paid_tickets_last_7d,
            snapshot.interested_fans,
            snapshot.city_signal_fans,
            snapshot.beacon_partners,
        ),
        // Each lever is intentionally one-shot per event. If a later policy wants
        // another wave it should become a distinct lever, not an accidental retry.
        action_idempotency_key: format!(
            "action:show-growth:{}:{}",
            snapshot.event_id,
            lever.as_str()
        ),
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
