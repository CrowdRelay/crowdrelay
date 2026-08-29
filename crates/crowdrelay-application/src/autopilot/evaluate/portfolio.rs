//! Portfolio optimization for growth intelligence dispatch.
//!
//! Extracts the portfolio candidate construction and optimizer call
//! from the main evaluator loop. The portfolio optimizer selects the
//! optimal set of dispatch candidates, accounting for audience overlap
//! and fatigue.

use std::collections::HashSet;

use crowdrelay_brain::{
    OpportunityAction, OpportunityId, PortfolioCandidate, PortfolioOptimizer, PortfolioSelection,
    context_hash,
};

use crate::autopilot::model::DecisionCandidate;

/// Builds portfolio candidates from scored growth intelligence candidates
/// and runs the optimizer to select the optimal dispatch set.
///
/// Returns the portfolio selection. The caller should only dispatch
/// candidates whose `decision_key` appears in the selection's `selected`
/// list, and skip all candidates if `do_nothing` is true.
#[must_use]
pub(super) fn select_portfolio(
    scored: &[(
        DecisionCandidate,
        crowdrelay_brain::DispatchPrediction,
        f64,
        usize,
    )],
) -> PortfolioSelection {
    let candidates: Vec<PortfolioCandidate> = scored
        .iter()
        .map(|(c, p, efe, _)| {
            let audience_key = format!("template:{}", p.template_id);
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
                cost: 1,
                source_context: c.decision_kind.to_string(),
                action_key: c.decision_key.clone(),
            }
        })
        .collect();
    let optimizer = PortfolioOptimizer::default();
    optimizer.select(candidates)
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
