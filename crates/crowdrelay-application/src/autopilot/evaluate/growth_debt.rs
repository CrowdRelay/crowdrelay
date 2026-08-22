//! Growth-debt candidate mapping. The domain decides whether neglected work is
//! now worth an operator's attention; this module only turns that finding into
//! a durable, idempotent Autopilot action under the usual authority gate.

use crowdrelay_domain::growth_debt::{
    GrowthDebtDecision, GrowthDebtObservation, evaluate_growth_debt,
};
use time::OffsetDateTime;

use super::{policy_evidence, *};

/// Buckets how far past its horizon the work is, so a debt ageing by an hour
/// does not mint a new decision every cycle while a materially worse one still
/// does. Coarser than the metric bucket on purpose: debt ages continuously and
/// every subject would otherwise cross a boundary eventually.
const fn overdue_bucket(overdue_basis_points: u32) -> u32 {
    overdue_basis_points / 5_000
}

/// Index of the cooldown window `now` falls in. Gives the action key a coarse
/// time component so the same debt can legitimately recur later without the
/// evaluator being able to raise it twice inside one cooldown.
fn cooldown_window(now: OffsetDateTime, cooldown_hours: u32) -> i64 {
    let window_seconds = i64::from(cooldown_hours.max(1)).saturating_mul(3_600);
    now.unix_timestamp().div_euclid(window_seconds)
}

pub(super) fn growth_debt_candidate(
    observation: &GrowthDebtObservation,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::GrowthDebt(domain_policy) = policy.config else {
        return Ok(None);
    };
    let GrowthDebtDecision::Raise(item) = evaluate_growth_debt(observation, domain_policy) else {
        return Ok(None);
    };
    let disposition = disposition(
        policy.autonomy_level,
        item.confidence,
        policy.minimum_confidence,
    );
    let subject = ActionSubject::from(observation.subject);
    let action = AutopilotActionPayload::RaiseGrowthDebt {
        subject_kind: subject.kind().to_owned(),
        subject_id: subject.uuid(),
        debt_kind: item.kind,
        recommended_action: item.kind.recommended_action().to_owned(),
        overdue_basis_points: item.overdue_basis_points,
        outstanding_items: item.outstanding_items,
        tracked_items: observation.tracked_items,
        priority: item.priority,
        template_key: item.kind.template_key().to_owned(),
    };
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject,
        decision_kind: item.kind.decision_kind(),
        confidence: item.confidence,
        disposition,
        reason: item.kind.reason(),
        input_snapshot: serde_json::to_value(observation)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action,
        // The debt kind is part of the key: one event can owe both skipped
        // levers and a stalled release plan, and those are separate findings
        // about the same subject rather than one finding to overwrite.
        decision_key: format!(
            "decision:growth-debt:v{}:{}:{}:{}:{}:{}",
            policy.version,
            subject.kind(),
            subject.uuid(),
            item.kind.as_str(),
            overdue_bucket(item.overdue_basis_points),
            item.outstanding_items,
        ),
        // One item per subject, debt kind and cooldown window. Not permanently
        // stable: a relationship revived in March and gone quiet again in
        // September is two pieces of work, and a forever-stable key would
        // silently swallow the second. Within a window the overdue ratio is
        // deliberately absent so ordinary ageing cannot stack duplicates on the
        // operator queue.
        action_idempotency_key: format!(
            "action:growth-debt:{}:{}:{}:{}",
            subject.kind(),
            subject.uuid(),
            item.kind.as_str(),
            cooldown_window(now, domain_policy.cooldown_hours),
        ),
    }))
}
