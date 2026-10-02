//! The candidate wrapper around `evaluate_growth_intelligence` — split
//! out of `growth_intelligence.rs` so the evaluator stays under the
//! source-size ratchet.
//!
//! One candidate per workspace-wide template (or per target community,
//! which `community_engager.rs` owns): the request the evaluator picked
//! becomes a `ScoredCandidate` carrying the decision payload, the
//! prediction made at decision time, and the evidence-gated disposition
//! that stands between the request and unattended execution.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(in crate::autopilot::evaluate) fn growth_intelligence_candidate(
    blocked_on_membership: &mut Vec<(String, u32)>,
    rooms_unread: &mut Vec<String>,
    snapshot: &GrowthIntelligenceSnapshot,
    policy: &AutopilotPolicy,
    evidence: ContextEvidence,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
    causal_model: &CausalModel,
    strategy: GrowthStrategy,
    exploration_novelty: f64,
) -> Result<Vec<ScoredCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::GrowthIntelligence(ref domain_policy) = policy.config else {
        return Ok(Vec::new());
    };
    // community-engager produces one candidate per target community.
    // Each community is a distinct experimental unit (TargetCommunity),
    // with its own decision_key, idempotency key, and prediction context.
    // This enables per-community randomized holdout: "does engaging r/djent
    // produce incremental durable fans versus not engaging r/djent?"
    if snapshot.template_id == "community-engager" {
        return community_engager_candidates(
            snapshot,
            policy,
            domain_policy,
            evidence,
            workspace_id,
            now,
            causal_model,
            strategy,
            exploration_novelty,
            blocked_on_membership,
            rooms_unread,
        );
    }
    // All other templates: 0 or 1 workspace-wide candidate.
    let Some(request) = evaluate_growth_intelligence(
        snapshot,
        domain_policy,
        causal_model,
        strategy,
        exploration_novelty,
        now,
    ) else {
        return Ok(Vec::new());
    };
    Ok(vec![candidate_from_request(
        &request,
        snapshot,
        policy,
        domain_policy,
        evidence,
        workspace_id,
        now,
    )?])
}

/// Builds a `ScoredCandidate` from an `IntelligenceRequest` for non-community
/// templates. The decision_key and idempotency key are workspace-wide
/// (template + cooldown window).
fn candidate_from_request(
    request: &IntelligenceRequest,
    snapshot: &GrowthIntelligenceSnapshot,
    policy: &AutopilotPolicy,
    domain_policy: &GrowthIntelligencePolicy,
    evidence: ContextEvidence,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<ScoredCandidate, serde_json::Error> {
    let prediction = request.prediction.clone();
    let efe_score = request.efe_score;
    let strategy_rank = request.strategy_rank;
    let treatment_stats = request.treatment_stats.clone();
    // `Confidence::MAX` is asserted, not measured: this context always
    // believes its own request is worth making. The evidence gate is the only
    // thing standing between that constant and unattended execution.
    //
    // The dispatch itself is internal work — its outcomes re-gate on their
    // own rows — so `require_approval` upgrades to auto-execution rather than
    // parking an unreviewable "approve this prompt" card in front of the
    // operator. `Deny`/`Observe`/`Recommend` still apply unchanged.
    let disposition =
        crowdrelay_domain::autonomy::internal_work_disposition(disposition_with_evidence(
            policy.autonomy_level,
            Confidence::MAX,
            policy.minimum_confidence,
            evidence,
            RATE_FLOOR,
        ));
    let action = AutopilotActionPayload::RequestAgentRun {
        template_id: request.template_id.to_owned(),
        prompt: request.prompt.clone(),
        priority: request.priority,
        tier: request.tier,
    };
    Ok(ScoredCandidate {
        candidate: DecisionCandidate {
            context: policy.context,
            subject: ActionSubject::Workspace(workspace_id),
            decision_kind: "request_agent_run",
            confidence: Confidence::MAX,
            disposition,
            reason: request.reason,
            input_snapshot: serde_json::json!({
                "snapshot": snapshot,
                "prediction": &request.prediction,
            }),
            policy_snapshot: policy_evidence(policy, domain_policy)?,
            action,
            decision_key: format!(
                "decision:growth-intelligence:v{}:{}:{}",
                policy.version,
                request.template_id,
                cooldown_window(now, request.key_window_hours),
            ),
            action_idempotency_key: format!(
                "action:agent-run:{}:{}",
                request.template_id,
                cooldown_window(now, request.key_window_hours),
            ),
        },
        prediction,
        efe_score,
        strategy_rank,
        treatment_stats,
        information_gain: request.information_gain,
        novelty: request.novelty,
    })
}
