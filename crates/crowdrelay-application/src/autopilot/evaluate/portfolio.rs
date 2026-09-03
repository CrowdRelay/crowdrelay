//! Portfolio optimization for growth intelligence dispatch.
//!
//! Extracts the portfolio candidate construction and optimizer call
//! from the main evaluator loop. The portfolio optimizer selects the
//! optimal set of dispatch candidates, accounting for audience overlap
//! and fatigue.
//!
//! Each candidate carries a `DecisionValue` — the canonical intrinsic
//! value object. The optimizer computes marginal value from it after
//! applying portfolio interactions (overlap, fatigue, budget).

use std::collections::HashSet;

use crowdrelay_brain::{
    DecisionMode, DecisionValue, EfeSignal, GrowthIntelligencePolicy, OpportunityAction,
    OpportunityId, PortfolioCandidate, PortfolioConfig, PortfolioOptimizer, PortfolioSelection,
    ResourceCost, WaitCandidateValue, context_hash,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::worker_template::{TemplateAudience, WorkerTemplate};

use crate::autopilot::evaluate::growth_intelligence::ScoredCandidate;
use crate::autopilot::model::DecisionCandidate;

/// P1-e: Extracts the audience identity from a candidate's decision_key.
///
/// The audience_key must be target-only (not template+target) so that two
/// different templates hitting the same community are detected as audience
/// overlap. The action differs; the audience doesn't.
///
/// - community-engager: `community:{target_id}` (extracted from decision_key
///   segment 4)
/// - workspace-wide templates: `workspace:{workspace_id}` (they all hit the
///   same audience — the workspace)
/// - other templates: `target:{decision_key}` (fallback — each decision_key
///   is a unique target)
fn audience_key_for(candidate: &DecisionCandidate, workspace_id: WorkspaceId) -> String {
    let parts: Vec<&str> = candidate.decision_key.split(':').collect();
    // decision:growth-intelligence:v{N}:{template}:{target_id}:{bucket}
    let template = parts.get(3).and_then(|id| WorkerTemplate::parse(id));
    match template.map(WorkerTemplate::audience) {
        Some(TemplateAudience::Community) => match parts.get(4) {
            Some(target_id) => format!("community:{target_id}"),
            // A community template with no target in its key is malformed.
            // Falling back to the decision key keeps it uniquely identified
            // rather than pooling it with an unrelated community.
            None => format!("target:{}", candidate.decision_key),
        },
        // Every workspace-wide dispatch reaches the same audience — the
        // band's own — so they share a key and the overlap penalty applies
        // between them. This used to be seven string literals, and
        // `discord-poster` was not among them: a discord post would have
        // counted as reaching different people than the telegram and social
        // posts going to the same channels on the same day.
        Some(TemplateAudience::Workspace) => {
            format!("workspace:{}", workspace_id.into_uuid())
        }
        // A scan reaches nobody, so it fatigues nobody. Its own key keeps it
        // out of the band audience's overlap accounting in both directions:
        // a scan does not suppress the posts behind it, and posts do not
        // suppress a scan.
        Some(TemplateAudience::Intelligence) => match template {
            Some(t) => format!("intelligence:{}", t.as_str()),
            None => format!("target:{}", candidate.decision_key),
        },
        // Not a growth-intelligence template: each decision key is its own
        // target.
        None => format!("target:{}", candidate.decision_key),
    }
}

/// Returns the operator-configured resource cost for a template, as a
/// `ResourceCost` with `CostSource::Configured`. Falls back to 1.0 when
/// the template is not in the policy's cost map.
fn template_cost(policy: &GrowthIntelligencePolicy, template_id: &str) -> ResourceCost {
    ResourceCost::configured(policy.template_cost(template_id))
}

/// Builds portfolio candidates from scored growth intelligence candidates
/// and runs the optimizer to select the optimal dispatch set.
///
/// Each candidate is constructed with a `DecisionValue` — the canonical
/// intrinsic value object that carries the full provenance trail:
/// estimation regime, evidence quality, uncertainty, bridge confidence.
/// The optimizer reads `decision_value.total()` for ranking and applies
/// portfolio interactions (overlap, fatigue, budget) to compute marginal
/// value.
///
/// `pending_measurement_count` is the number of unresolved evidence rows
/// (dispatches whose outcomes haven't been observed yet). This feeds the
/// WAIT candidate's value-of-information computation. When > 0, WAIT
/// has real epistemic value — the brain can learn from pending outcomes
/// before committing to new dispatches.
///
/// Returns the portfolio selection. The caller should only dispatch
/// candidates whose `decision_key` appears in the selection's `selected`
/// list, and skip all candidates if `do_nothing` is true.
#[must_use]
pub(super) fn select_portfolio(
    scored: &[ScoredCandidate],
    policy: &GrowthIntelligencePolicy,
    pending_measurement_count: u32,
    workspace_id: WorkspaceId,
    experimental_keys: &std::collections::HashSet<String>,
) -> PortfolioSelection {
    let candidates: Vec<PortfolioCandidate> = scored
        .iter()
        .map(|(c, p, efe, _, stats)| {
            // P1-e: audience_key is target-only, not template+target.
            // Two different templates hitting the same community share
            // an audience_key, so the overlap penalty applies correctly.
            let audience_key = audience_key_for(c, workspace_id);
            // Construct the canonical DecisionValue from treatment-aware
            // stats. This is the single source of truth for all value
            // semantics — the optimizer reads decision_value.total()
            // and applies portfolio interactions to compute marginal
            // value.
            let decision_mode = if stats.use_treatment_effect {
                DecisionMode::Exploit
            } else {
                DecisionMode::Explore
            };
            let decision_value = DecisionValue::from_stats(
                stats,
                template_cost(policy, &p.template_id),
                decision_mode,
            );
            PortfolioCandidate {
                opportunity_id: OpportunityId {
                    template_id: p.template_id.clone(),
                    target: c.decision_key.clone(),
                    action: OpportunityAction::from_template(&p.template_id),
                    context_hash: context_hash(&p.context),
                },
                // EFE is a candidate-generation signal, NOT an economic
                // value. The optimizer ignores generation_signal for
                // ranking — DecisionValue.total() is the sole authority.
                generation_signal: Some(EfeSignal {
                    information_gain: 0.0, // Wired when EFE carries this
                    novelty: 0.0,
                    efe_score: *efe,
                }),
                audience_key,
                source_context: c.decision_kind.to_string(),
                action_key: c.decision_key.clone(),
                // P0-2: is_experimental is true for treatment-assigned
                // candidates from active experiments. These candidates
                // can use the experimental_dispatch_budget (additional
                // slots beyond max_dispatches) when the VOI justifies it.
                is_experimental: experimental_keys.contains(&c.decision_key),
                decision_value,
            }
        })
        .collect();
    // Compute WAIT candidate value. Every term is in expected Y30 fans.
    // The WAIT candidate's opportunity_cost = -best_action_y30, and
    // wait_total = VOI + fatigue + option - best_action_y30.
    // WAIT wins when wait_total > 0 (net positive utility).
    let best_y30 = candidates
        .iter()
        .map(|c| c.decision_value.total())
        .fold(0.0_f64, f64::max);
    let avg_treatment_std = {
        let stds: Vec<f64> = candidates
            .iter()
            .filter(|c| c.decision_value.uncertainty > 0.0)
            .map(|c| c.decision_value.uncertainty)
            .collect();
        if stds.is_empty() {
            0.0
        } else {
            stds.iter().sum::<f64>() / stds.len() as f64
        }
    };
    // Phase 1: VOI uses the pending measurement count passed by the
    // caller. Fatigue recovery is 0.0 (not yet computed pre-portfolio).
    // The WAIT candidate competes via opportunity cost + VOI: if the
    // best action has low expected Y30 and there are pending measurements
    // whose outcomes could inform the decision, WAIT can win.
    let wait =
        WaitCandidateValue::compute(best_y30, pending_measurement_count, avg_treatment_std, 0.0);
    // P0-2: Wire the experimental dispatch budget from the policy into the
    // optimizer config. This allows additional treatment dispatches beyond
    // max_dispatches when the candidate is part of an active experiment.
    let config = PortfolioConfig {
        experimental_dispatch_budget: policy.experimental_dispatch_budget,
        ..Default::default()
    };
    let optimizer = PortfolioOptimizer { config };
    optimizer.select_with_wait(candidates, wait)
}

/// Extracts the selected decision keys from a portfolio selection for
/// fast lookup during the dispatch loop.
#[must_use]
pub(super) fn selected_keys(selection: &PortfolioSelection) -> HashSet<String> {
    selection
        .selected
        .iter()
        .map(|c| c.opportunity_id.target.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::model::{ActionSubject, AutopilotActionPayload, AutopilotContext};
    use crowdrelay_brain::AgentTier;
    use crowdrelay_domain::autonomy::{Confidence, PolicyDisposition};

    fn candidate(decision_key: &str) -> DecisionCandidate {
        DecisionCandidate {
            context: AutopilotContext::GrowthIntelligence,
            subject: ActionSubject::Workspace(WorkspaceId::from_uuid(uuid::Uuid::nil())),
            decision_kind: "request_agent_run",
            confidence: Confidence::MAX,
            disposition: PolicyDisposition::AutoExecute,
            reason: "fixture",
            input_snapshot: serde_json::json!({}),
            policy_snapshot: serde_json::json!({}),
            action: AutopilotActionPayload::RequestAgentRun {
                template_id: "fixture".to_owned(),
                prompt: String::new(),
                priority: 1,
                tier: AgentTier::Basic,
            },
            decision_key: decision_key.to_owned(),
            action_idempotency_key: decision_key.to_owned(),
        }
    }

    fn key_for(template: &str) -> String {
        let ws = WorkspaceId::from_uuid(uuid::Uuid::nil());
        audience_key_for(
            &candidate(&format!(
                "decision:growth-intelligence:v1:{template}:target:0"
            )),
            ws,
        )
    }

    #[test]
    fn everything_that_posts_to_the_band_shares_one_audience() {
        // The overlap penalty is what stops three posts to the same people on
        // the same day counting as three separate reaches.
        let expected = key_for("social-post");
        for template in [
            "press-pitch",
            "telegram-poster",
            "discord-poster",
            "signal-inviter",
        ] {
            assert_eq!(key_for(template), expected, "{template}");
        }
        assert!(expected.starts_with("workspace:"));
    }

    #[test]
    fn a_scan_does_not_share_an_audience_with_a_post() {
        // Scanners reached nobody but carried the band's audience key, so one
        // selected scan cut every posting template behind it by 37%, then
        // 68%, then 93%, while community candidates kept full value.
        let post = key_for("social-post");
        for scanner in [
            "reddit-scanner",
            "telegram-scanner",
            "metal-archives-scanner",
            "bandcamp-scanner",
            "growth-strategist",
        ] {
            assert_ne!(
                key_for(scanner),
                post,
                "{scanner} must not fatigue the band"
            );
        }
    }

    #[test]
    fn two_scanners_do_not_fatigue_each_other_either() {
        assert_ne!(key_for("reddit-scanner"), key_for("telegram-scanner"));
    }

    #[test]
    fn each_community_is_its_own_audience() {
        let ws = WorkspaceId::from_uuid(uuid::Uuid::nil());
        let a = audience_key_for(
            &candidate("decision:growth-intelligence:v1:community-engager:aaa:0"),
            ws,
        );
        let b = audience_key_for(
            &candidate("decision:growth-intelligence:v1:community-engager:bbb:0"),
            ws,
        );
        assert_eq!(a, "community:aaa");
        assert_ne!(a, b);
    }

    #[test]
    fn a_decision_key_from_another_context_is_its_own_target() {
        let ws = WorkspaceId::from_uuid(uuid::Uuid::nil());
        let key = audience_key_for(&candidate("decision:plays:v1:something:else"), ws);
        assert!(key.starts_with("target:"), "{key}");
    }
}
