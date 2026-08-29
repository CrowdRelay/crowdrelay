//! Portfolio optimization for growth intelligence dispatch.
//!
//! Extracts the portfolio candidate construction and optimizer call
//! from the main evaluator loop. The portfolio optimizer selects the
//! optimal set of dispatch candidates, accounting for audience overlap
//! and fatigue.

use std::collections::HashSet;

use crowdrelay_brain::{
    DecisionMode, GrowthIntelligencePolicy, OpportunityAction, OpportunityId, PortfolioCandidate,
    PortfolioOptimizer, PortfolioSelection, ResourceCost, WaitCandidateValue, context_hash,
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
/// The portfolio optimizer now uses the treatment-aware stats (Y30
/// durable fans, treatment confidence, P(τ > δ)) as the primary value
/// signal when the treatment-effect model has sufficient confidence.
/// When confidence is low, it falls back to the EFE score (which uses
/// the outcome model's expected fans).
///
/// Returns the portfolio selection. The caller should only dispatch
/// candidates whose `decision_key` appears in the selection's `selected`
/// list, and skip all candidates if `do_nothing` is true.
#[must_use]
pub(super) fn select_portfolio(
    scored: &[ScoredCandidate],
    policy: &GrowthIntelligencePolicy,
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
            // Wire the North Star fields from the treatment-aware stats.
            // When Y30 is confident, expected_durable_fans uses the Y30
            // treatment effect. When only Y14 is confident, it uses the
            // Y14 treatment effect with bridge-inflated uncertainty. When
            // neither is confident, it falls back to 0.0 (the optimizer
            // will use the EFE score instead).
            let expected_durable_fans = if stats.use_treatment_effect {
                if stats.uses_y30 {
                    stats.treatment_effect_y30
                } else {
                    stats.treatment_effect
                }
            } else {
                0.0
            };
            let treatment_confidence = if stats.uses_y30 {
                stats.treatment_confidence_y30
            } else {
                stats.treatment_confidence
            };
            let treatment_std = if stats.uses_y30 {
                stats.treatment_std_y30
            } else {
                stats.treatment_std
            };
            PortfolioCandidate {
                opportunity_id: OpportunityId {
                    template_id: p.template_id.clone(),
                    target: c.decision_key.clone(),
                    action: OpportunityAction::from_template(&p.template_id),
                    context_hash: context_hash(&p.context),
                },
                efe_score: *efe,
                expected_fans: p.expected_new_fans,
                audience_key,
                resource_cost: template_cost(policy, &p.template_id),
                source_context: c.decision_kind.to_string(),
                action_key: c.decision_key.clone(),
                expected_durable_fans,
                treatment_confidence,
                treatment_std,
                p_meaningful_effect: stats.p_meaningful_effect,
                decision_mode: if stats.use_treatment_effect {
                    DecisionMode::Exploit
                } else {
                    DecisionMode::Explore
                },
            }
        })
        .collect();
    // Compute WAIT candidate value. Every term is in expected Y30 fans.
    // Phase 1: conservative — VOI from pending measurements is not yet
    // wired (count=0), fatigue recovery is not yet computed (0.0).
    // The WAIT candidate still competes via opportunity cost: if the
    // best action candidate has very low expected Y30, WAIT can win.
    let best_y30 = candidates
        .iter()
        .map(|c| {
            if c.expected_durable_fans > 0.0 {
                c.expected_durable_fans
            } else {
                c.expected_fans
            }
        })
        .fold(0.0_f64, f64::max);
    let avg_treatment_std = {
        let stds: Vec<f64> = candidates
            .iter()
            .filter(|c| c.treatment_std > 0.0)
            .map(|c| c.treatment_std)
            .collect();
        if stds.is_empty() {
            0.0
        } else {
            stds.iter().sum::<f64>() / stds.len() as f64
        }
    };
    let wait = WaitCandidateValue::compute(best_y30, 0, avg_treatment_std, 0.0);
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
