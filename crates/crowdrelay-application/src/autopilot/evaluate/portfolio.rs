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
    DecisionMode, DecisionValue, GrowthIntelligencePolicy, OpportunityAction, OpportunityId,
    PortfolioCandidate, PortfolioOptimizer, PortfolioSelection, ResourceCost, WaitCandidateValue,
    context_hash,
};

use crate::autopilot::evaluate::growth_intelligence::ScoredCandidate;

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
) -> PortfolioSelection {
    let candidates: Vec<PortfolioCandidate> = scored
        .iter()
        .map(|(c, p, efe, _, stats)| {
            // Audience key includes both the template AND the target
            // (decision_key). Previously this was just `template:{}`,
            // which meant two dispatches to different subreddits with
            // the same template were treated as the same audience —
            // causing the portfolio optimizer to reject the second as
            // "audience overlap" when the audiences are actually
            // disjoint (e.g. r/MetalMusic and r/progmetal are different
            // communities).
            let audience_key = format!("template:{}:{}", p.template_id, c.decision_key);
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
                efe_score: *efe,
                audience_key,
                source_context: c.decision_kind.to_string(),
                action_key: c.decision_key.clone(),
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
    let optimizer = PortfolioOptimizer::default();
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
