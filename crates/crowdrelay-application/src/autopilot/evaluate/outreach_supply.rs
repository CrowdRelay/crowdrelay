//! The one decision that keeps the pitcher from looping over an empty table.
//!
//! Every outreach capability the executor advertises reads from
//! `viryaos_outreach_targets`. Discovery fills that table, but discovery has
//! only ever been an inbound endpoint: something outside had to decide to run
//! it. That made zero targets a stable state rather than a problem the agent
//! could notice, which is the difference between a growth engine and a very
//! well-tested set of rules about growth.

use super::*;

pub(super) fn outreach_supply_candidate(
    snapshot: &OutreachSupplySnapshot,
    policy: &AutopilotPolicy,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::OutreachSupply(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let OutreachSupplyDecision::Request {
        requested_candidates,
        confidence,
    } = evaluate_outreach_supply(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    // The keys carry the sweep the request is a reaction to, not the clock.
    // Two cycles that see the same starved pipeline are the same decision, and
    // re-asking would spend an API call to learn what the last one already did.
    let last_sweep = snapshot
        .last_sweep_requested_at
        .map_or(0, OffsetDateTime::unix_timestamp);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::Workspace(workspace_id),
        decision_kind: "replenish_outreach_supply",
        confidence,
        disposition,
        reason: "the pitcher has fewer confirmed submission routes than the policy floor",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action: AutopilotActionPayload::RequestOutreachDiscovery {
            requested_candidates,
        },
        decision_key: format!(
            "decision:outreach-supply:v{}:{}:{}:{}",
            policy.version, snapshot.pitchable_targets, snapshot.admitted_candidates, last_sweep
        ),
        action_idempotency_key: format!(
            "action:outreach-supply:{}:{}:{}",
            snapshot.pitchable_targets, snapshot.admitted_candidates, last_sweep
        ),
    }))
}
