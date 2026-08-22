//! External-metric candidate mapping. The domain decides whether a movement is
//! worth an operator's attention; this module only turns that finding into a
//! durable, idempotent Autopilot action under the usual authority gate.

use crowdrelay_domain::growth_metrics::{
    GrowthMetricDecision, GrowthMetricSnapshot, evaluate_growth_metric,
};
use time::OffsetDateTime;

use super::{policy_evidence, *};

/// Buckets the deviation so ordinary drift inside one finding does not mint a
/// new decision every cycle, while a materially larger deviation still does.
const fn deviation_bucket(deviation_basis_points: u32) -> u32 {
    deviation_basis_points / 2_500
}

/// Index of the cooldown window `now` falls in. Gives the action key a coarse
/// time component so the same finding can legitimately recur later without the
/// evaluator being able to raise it twice inside one cooldown.
fn cooldown_window(now: OffsetDateTime, cooldown_hours: u32) -> i64 {
    let window_seconds = i64::from(cooldown_hours.max(1)).saturating_mul(3_600);
    now.unix_timestamp().div_euclid(window_seconds)
}

pub(super) fn growth_metric_candidate(
    snapshot: &GrowthMetricSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::GrowthMetrics(domain_policy) = policy.config else {
        return Ok(None);
    };
    let GrowthMetricDecision::Raise(opportunity) =
        evaluate_growth_metric(snapshot, domain_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(
        policy.autonomy_level,
        opportunity.confidence,
        policy.minimum_confidence,
    );
    let action = AutopilotActionPayload::RaiseGrowthOpportunity {
        series_id: snapshot.series_id,
        platform: snapshot.platform,
        metric_key: snapshot.metric_key.clone(),
        signal: opportunity.signal,
        recommended_action: opportunity.signal.recommended_action().to_owned(),
        deviation_basis_points: opportunity.deviation_basis_points,
        priority: opportunity.priority,
        template_key: opportunity.signal.template_key().to_owned(),
    };
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::GrowthMetricSeries(snapshot.series_id),
        decision_kind: "raise_growth_metric_opportunity",
        confidence: opportunity.confidence,
        disposition,
        reason: opportunity.signal.reason(),
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action,
        decision_key: format!(
            "decision:growth-metric:v{}:{}:{}:{}:{}",
            policy.version,
            snapshot.series_id,
            opportunity.signal.as_str(),
            deviation_bucket(opportunity.deviation_basis_points),
            snapshot.trend.latest_value,
        ),
        // One item per series, signal and cooldown window. The key must not be
        // permanently stable: a stall resolved in March and a stall recurring
        // in September are two separate pieces of work, and a forever-stable
        // key would silently swallow the second one. Within a window the
        // deviation is deliberately absent from the key so ordinary drift
        // cannot stack duplicates on the operator queue.
        action_idempotency_key: format!(
            "action:growth-metric:{}:{}:{}",
            snapshot.series_id,
            opportunity.signal.as_str(),
            cooldown_window(now, domain_policy.cooldown_hours),
        ),
    }))
}
