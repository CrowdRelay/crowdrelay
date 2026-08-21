//! Attendance-growth candidate mapping. The domain decides the lever; this module
//! only turns that decision into a durable, idempotent Autopilot action.

use crowdrelay_domain::show_growth::{
    ShowGrowthDecision, ShowGrowthSnapshot, evaluate_show_growth,
};
use time::OffsetDateTime;

use super::{policy_evidence, *};

pub(super) fn show_growth_candidate(
    snapshot: ShowGrowthSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::ShowGrowth(domain_policy) = policy.config else {
        return Ok(None);
    };
    let ShowGrowthDecision::Request { lever, confidence } =
        evaluate_show_growth(snapshot, domain_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let action = AutopilotActionPayload::RequestShowGrowth {
        event_id: snapshot.event_id,
        lever,
        template_key: lever.template_key().to_owned(),
    };
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::Event(snapshot.event_id),
        decision_kind: "activate_show_growth_lever",
        confidence,
        disposition,
        reason: "a bounded attendance-growth lever is due from first-party show evidence",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
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
    }))
}
