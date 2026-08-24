//! The booking pipeline's supply request, split from outreach supply so the
//! two starvation rules stay independently readable.

use super::*;

/// The booking pipeline's supply request. Same starvation rule, different
/// shelves: the negotiation machinery is complete and this is what keeps its
/// target table from being a stable zero.
pub(super) fn booking_supply_candidate(
    snapshot: &crowdrelay_domain::booking_discovery::BookingSupplySnapshot,
    policy: &AutopilotPolicy,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::BookingOpportunity(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let crowdrelay_domain::booking_discovery::BookingSupplyDecision::Request { requested_count } =
        crowdrelay_domain::booking_discovery::evaluate_booking_supply(
            *snapshot,
            domain_policy.supply,
        )
    else {
        return Ok(None);
    };
    let confidence = Confidence::saturating_from_basis_points(8_800);
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let last_request = snapshot
        .hours_since_last_request
        .map_or(0, |hours| now.unix_timestamp() - i64::from(hours) * 3600);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::Workspace(workspace_id),
        decision_kind: "request_booking_target_discovery",
        confidence,
        disposition,
        reason: "the booking pipeline has fewer contactable targets than the policy floor",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::RequestBookingTargetDiscovery { requested_count },
        decision_key: format!(
            "decision:booking-supply:v{}:{}:{}",
            policy.version, snapshot.active_eligible_targets, last_request
        ),
        // One request per cooldown window: the keys carry the last request's
        // position in time, so re-asking inside the cooldown dedupes away.
        action_idempotency_key: format!(
            "action:booking-supply:{}:{}",
            snapshot.active_eligible_targets, last_request
        ),
    }))
}
