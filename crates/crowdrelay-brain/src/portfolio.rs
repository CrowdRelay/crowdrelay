//! Portfolio optimizer — global candidate pool with submodular greedy selection.
//!
//! The brain doesn't dispatch candidates context-by-context — it collects ALL
//! candidates from ALL contexts into a global pool and selects the optimal
//! portfolio. This is the "GET FANS" optimizer: it maximizes expected
//! incremental fans across the entire action space, subject to budget
//! constraints, audience overlap, and fatigue.
//!
//! # Why a portfolio optimizer?
//!
//! Each context (GrowthIntelligence, Plays, Outreach, etc.) produces
//! candidates independently. Without a global optimizer:
//! - Two contexts might dispatch to the same subreddit on the same day
//!   (audience overlap → diminishing returns)
//! - The brain might dispatch 10 low-value candidates instead of 3 high-value
//!   ones (budget misallocation)
//! - "DO NOTHING" is never a candidate (the brain always dispatches something
//!   even when the expected value is negative)
//!
//! # Algorithm: greedy with marginal value
//!
//! Fan acquisition has diminishing returns (posting to the same subreddit
//! twice in one day doesn't double your fans). The objective function is
//! **approximately submodular** — the audience overlap penalty creates
//! diminishing returns, which is the key property for greedy selection.
//!
//! # Honesty about submodularity
//!
//! The (1 - 1/e) ≈ 63% approximation guarantee for greedy selection requires
//! **strict** submodularity. Our objective is approximately submodular:
//!
//! - **Overlap penalty** (diminishing returns for same audience): submodular ✓
//! - **Fatigue** (each additional dispatch to the same audience is less
//!   effective): submodular ✓
//! - **Cost budget** (knapsack constraint): compatible with greedy ✓
//! - **Network propagation** (fans from one audience spread to connected
//!   audiences): **supermodular** ✗ — this breaks the guarantee.
//!
//! In practice, network propagation effects are small compared to overlap
//! and fatigue, so the objective is *close* to submodular. The greedy
//! algorithm still produces good solutions, but the 63% guarantee is
//! approximate, not exact. For a formal guarantee, one would need to either
//! remove network propagation or use a more sophisticated algorithm (e.g.,
//! the multilinear extension relaxation).
//!
//! The algorithm:
//!
//! 1. Start with an empty portfolio.
//! 2. At each step, add the candidate with the highest marginal value
//!    (expected fans minus overlap penalty and fatigue with already-selected
//!    candidates).
//! 3. Stop when the marginal value drops below `min_marginal_value`, the
//!    dispatch count budget is exhausted, or the cost budget is exhausted.
//! 4. "DO NOTHING" is always a candidate — if all marginal values are negative,
//!    the brain does nothing.

use serde::{Deserialize, Serialize};

use crate::opportunity::OpportunityId;

/// The decision mode for a portfolio candidate — why the brain is
/// dispatching this candidate.
///
/// This explicit mode helps the brain reason about its decisions:
/// - **Exploit**: dispatching because the expected value is high.
/// - **Learn**: dispatching because the uncertainty is high (information gain).
/// - **Explore**: dispatching because the candidate is novel (Go-Explore).
/// - **DoNothing**: not dispatching (the candidate was rejected).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionMode {
    /// Exploit: dispatching for expected fan growth (high expected value).
    #[default]
    Exploit,
    /// Learn: dispatching for information gain (high uncertainty, low confidence).
    Learn,
    /// Explore: dispatching for novelty (Go-Explore bonus).
    Explore,
    /// DoNothing: the candidate was not selected.
    DoNothing,
}

/// A candidate in the global pool, scored and ready for portfolio selection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortfolioCandidate {
    /// The opportunity identity (stable across cycles).
    pub opportunity_id: OpportunityId,
    /// The EFE score (lower = better). Used as the base value.
    pub efe_score: f64,
    /// The expected incremental fans from this candidate.
    pub expected_fans: f64,
    /// The audience key — candidates with the same audience key overlap.
    /// E.g. "subreddit:r_MetalMusic" or "venue:Warsaw_Palladium".
    pub audience_key: String,
    /// The cost in "dispatch budget" units (typically 1 per dispatch).
    ///
    /// Not yet enforced by [`PortfolioOptimizer::select`], which currently
    /// only caps `max_dispatches`. Reserved for a future cost-budget
    /// constraint so the candidate shape is stable before the constraint
    /// is wired in.
    pub cost: u32,
    /// The context that produced this candidate (for tracing).
    pub source_context: String,
    /// The action payload key (for linking to the persist layer).
    pub action_key: String,
    /// The Y30 (North Star) expected durable fans. When available, the
    /// portfolio optimizer uses this instead of `expected_fans` for ranking
    /// and marginal value computation (P1.4).
    pub expected_durable_fans: f64,
    /// The treatment-effect confidence (paired observation count). Used by
    /// the knowledge gradient computation (P1.4).
    pub treatment_confidence: u32,
    /// The treatment-effect std. Used by the knowledge gradient (P1.4).
    pub treatment_std: f64,
    /// The P(τ > δ) meaningful-effect probability. Used as a tie-breaker
    /// in portfolio selection (P1.4).
    pub p_meaningful_effect: f64,
    /// The decision mode — why the brain is dispatching this candidate.
    /// Computed by the portfolio optimizer during selection (P2.6).
    pub decision_mode: DecisionMode,
}

/// The result of portfolio optimization.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PortfolioSelection {
    /// The selected candidates, in dispatch priority order.
    pub selected: Vec<PortfolioCandidate>,
    /// The candidates that were not selected (and why).
    pub rejected: Vec<PortfolioRejection>,
    /// The total expected fans from the selected portfolio.
    pub total_expected_fans: f64,
    /// Whether "DO NOTHING" was selected (all candidates had negative value).
    pub do_nothing: bool,
}

/// A rejected candidate and the reason for rejection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortfolioRejection {
    pub opportunity_key: String,
    pub reason: RejectionReason,
}

/// Why a candidate was not selected for the portfolio.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionReason {
    /// The marginal value was negative (not worth dispatching).
    NegativeMarginalValue,
    /// The marginal value was positive but below the minimum threshold.
    BelowThreshold,
    /// The dispatch count budget (max_dispatches) was exhausted.
    MaxDispatchesReached,
    /// The cost budget was exhausted.
    BudgetExhausted,
    /// The candidate was superseded by a better candidate for the same audience.
    Superseded,
}

/// Configuration for the portfolio optimizer.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PortfolioConfig {
    /// The maximum number of dispatches per cycle.
    pub max_dispatches: u32,
    /// The maximum total cost per cycle. Candidates have a `cost` field;
    /// the optimizer stops when the total cost of selected candidates
    /// exceeds this budget. Set to 0 to disable cost budgeting (use only
    /// `max_dispatches`).
    pub cost_budget: u32,
    /// The audience overlap penalty: each additional candidate targeting the
    /// same audience gets its expected fans multiplied by (1 - penalty × count).
    /// 0.0 = no penalty, 1.0 = full penalty (second dispatch to same audience
    /// has zero value).
    pub audience_overlap_penalty: f64,
    /// The fatigue factor: each additional dispatch to the same audience
    /// gets its expected fans multiplied by `fatigue_decay^count`. This is
    /// separate from overlap — overlap models audience duplication, fatigue
    /// models audience burnout (seeing too many posts from the same artist).
    /// 1.0 = no fatigue, 0.8 = 20% reduction per additional dispatch.
    pub fatigue_decay: f64,
    /// The minimum marginal value required to add a candidate to the portfolio.
    /// Below this, the brain prefers DO NOTHING.
    pub min_marginal_value: f64,
}

impl Default for PortfolioConfig {
    fn default() -> Self {
        Self {
            max_dispatches: 5,
            cost_budget: 0, // 0 = disabled (use max_dispatches only)
            audience_overlap_penalty: 0.3,
            fatigue_decay: 0.9,
            min_marginal_value: 0.1,
        }
    }
}

/// The portfolio optimizer — selects the optimal set of candidates from the
/// global pool.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PortfolioOptimizer {
    pub config: PortfolioConfig,
}

impl PortfolioOptimizer {
    /// Creates a new optimizer with the given configuration.
    #[must_use]
    pub fn new(config: PortfolioConfig) -> Self {
        Self { config }
    }

    /// Selects the optimal portfolio from the global candidate pool.
    ///
    /// Uses greedy selection with marginal value:
    /// 1. Sort candidates by expected durable fans (North Star) when available,
    ///    falling back to expected fans.
    /// 2. At each step, pick the candidate with the highest marginal value
    ///    (expected fans minus audience overlap and fatigue with already-selected).
    /// 3. Stop when marginal value < min_marginal_value, dispatch count
    ///    budget exhausted, or cost budget exhausted.
    ///
    /// # North Star objective (P1.4)
    ///
    /// When `expected_durable_fans > 0`, the optimizer uses Y30 durable fans
    /// as the ranking signal instead of raw expected fans. This ensures the
    /// portfolio optimizes for the long-term North Star, not short-term
    /// incremental fans.
    #[must_use]
    pub fn select(&self, candidates: Vec<PortfolioCandidate>) -> PortfolioSelection {
        if candidates.is_empty() {
            return PortfolioSelection {
                do_nothing: true,
                ..Default::default()
            };
        }
        // Track audience usage for overlap + fatigue computation.
        let mut audience_counts: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();
        let mut selected: Vec<PortfolioCandidate> = Vec::new();
        let mut rejected: Vec<PortfolioRejection> = Vec::new();
        let mut remaining: Vec<PortfolioCandidate> = candidates;
        let mut total_cost: u32 = 0;
        // Sort by North Star (durable fans) when available, else expected fans.
        remaining.sort_by(|a, b| {
            let val_a = if a.expected_durable_fans > 0.0 {
                a.expected_durable_fans
            } else {
                a.expected_fans
            };
            let val_b = if b.expected_durable_fans > 0.0 {
                b.expected_durable_fans
            } else {
                b.expected_fans
            };
            val_b
                .partial_cmp(&val_a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut total_expected_fans = 0.0;
        while selected.len() < self.config.max_dispatches as usize && !remaining.is_empty() {
            // Find the candidate with the highest marginal value.
            let mut best_idx = None;
            let mut best_marginal = f64::NEG_INFINITY;
            for (i, candidate) in remaining.iter().enumerate() {
                // Check cost budget — skip candidates that would exceed it.
                if self.config.cost_budget > 0
                    && total_cost + candidate.cost > self.config.cost_budget
                {
                    continue;
                }
                let audience_count = audience_counts
                    .get(&candidate.audience_key)
                    .copied()
                    .unwrap_or(0);
                // Overlap penalty: diminishing returns for same audience.
                let overlap_factor =
                    1.0 - self.config.audience_overlap_penalty * audience_count as f64;
                // Fatigue: exponential decay for repeated dispatches.
                let fatigue_factor = self.config.fatigue_decay.powi(audience_count as i32);
                // Use North Star (durable fans) when available.
                let base_value = if candidate.expected_durable_fans > 0.0 {
                    candidate.expected_durable_fans
                } else {
                    candidate.expected_fans
                };
                let marginal = base_value * overlap_factor.max(0.0) * fatigue_factor;
                if marginal > best_marginal {
                    best_marginal = marginal;
                    best_idx = Some(i);
                }
            }
            // Check stopping conditions.
            // If no candidate fit the cost budget, reject the rest as
            // budget exhausted (not negative marginal value).
            if best_idx.is_none() {
                for candidate in remaining.drain(..) {
                    rejected.push(PortfolioRejection {
                        opportunity_key: candidate.opportunity_id.to_string(),
                        reason: RejectionReason::BudgetExhausted,
                    });
                }
                break;
            }
            if best_marginal < self.config.min_marginal_value {
                // All remaining candidates have negative or low marginal value.
                // Reject the rest and stop.
                for candidate in remaining.drain(..) {
                    rejected.push(PortfolioRejection {
                        opportunity_key: candidate.opportunity_id.to_string(),
                        reason: if best_marginal < 0.0 {
                            RejectionReason::NegativeMarginalValue
                        } else {
                            RejectionReason::BelowThreshold
                        },
                    });
                }
                break;
            }
            // Select the best candidate. swap_remove is O(1) — the
            // remaining order doesn't matter because we rescan each
            // iteration anyway.
            if let Some(idx) = best_idx {
                let candidate = remaining.swap_remove(idx);
                total_cost += candidate.cost;
                *audience_counts
                    .entry(candidate.audience_key.clone())
                    .or_insert(0) += 1;
                total_expected_fans += best_marginal;
                // Compute the decision mode (P2.6):
                // - Learn: low treatment confidence (< 10) → information gain
                // - Explore: high treatment std relative to expected value
                // - Exploit: high confidence, high expected value
                let mode = if candidate.treatment_confidence < 10 && candidate.treatment_std > 0.0 {
                    DecisionMode::Learn
                } else if candidate.treatment_std > best_marginal.abs().max(1.0) {
                    DecisionMode::Explore
                } else {
                    DecisionMode::Exploit
                };
                let mut candidate = candidate;
                candidate.decision_mode = mode;
                selected.push(candidate);
            } else {
                // No candidate fits the cost budget — reject the rest.
                for candidate in remaining.drain(..) {
                    rejected.push(PortfolioRejection {
                        opportunity_key: candidate.opportunity_id.to_string(),
                        reason: RejectionReason::BudgetExhausted,
                    });
                }
                break;
            }
        }
        // Reject any remaining candidates (max_dispatches was reached).
        for candidate in remaining {
            rejected.push(PortfolioRejection {
                opportunity_key: candidate.opportunity_id.to_string(),
                reason: RejectionReason::MaxDispatchesReached,
            });
        }
        let do_nothing = selected.is_empty();
        PortfolioSelection {
            selected,
            rejected,
            total_expected_fans,
            do_nothing,
        }
    }

    /// Computes the knowledge gradient (KG) for a candidate — the expected
    /// improvement in the portfolio's North Star objective from learning
    /// about this candidate.
    ///
    /// The KG captures the value of information: dispatching a candidate with
    /// high uncertainty but potentially high durable-fan effect teaches the
    /// brain about the Y30 North Star, improving future decisions.
    ///
    /// ```text
    /// KG = treatment_std × φ(z) + (treatment_effect - δ) × Φ(z)
    /// ```
    ///
    /// where z = (δ - treatment_effect) / treatment_std, and φ/Φ are the
    /// standard Normal PDF/CDF. This is the standard KG formula for a
    /// Normal posterior with measurement cost = 0.
    ///
    /// Candidates with high KG are worth dispatching even if their current
    /// expected value is low, because the brain learns from the outcome.
    #[allow(dead_code)] // TODO: wire into production path (next sprint)
    #[must_use]
    pub fn knowledge_gradient(candidate: &PortfolioCandidate) -> f64 {
        if candidate.treatment_std <= 0.0 {
            return 0.0;
        }
        // Use the meaningful-effect threshold δ = 1.0 (from causal_model).
        let delta = 1.0;
        let z = (delta - candidate.expected_durable_fans) / candidate.treatment_std;
        // Standard Normal PDF: φ(z) = exp(-z²/2) / √(2π)
        let phi = (-z * z / 2.0).exp() / (2.0 * std::f64::consts::PI).sqrt();
        // Standard Normal CDF: Φ(z) — using the existing normal_cdf from bayesian.
        let cdf = crate::bayesian::normal_cdf(z);
        // KG = σ × φ(z) + (μ - δ) × (1 - Φ(z))
        // = σ × φ(z) + (μ - δ) × Φ(-z)
        candidate.treatment_std * phi + (candidate.expected_durable_fans - delta) * (1.0 - cdf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::causal_model::DispatchContext;
    use crate::opportunity::OpportunityAction;

    fn make_candidate(
        template: &str,
        target: &str,
        expected_fans: f64,
        audience: &str,
    ) -> PortfolioCandidate {
        let ctx = DispatchContext::default();
        PortfolioCandidate {
            opportunity_id: OpportunityId::new(template, target, OpportunityAction::Post, &ctx),
            efe_score: -expected_fans,
            expected_fans,
            audience_key: audience.to_owned(),
            cost: 1,
            source_context: "GrowthIntelligence".to_owned(),
            action_key: format!("action:{template}:{target}"),
            expected_durable_fans: 0.0,
            treatment_confidence: 0,
            treatment_std: 0.0,
            p_meaningful_effect: 0.0,
            decision_mode: DecisionMode::Exploit,
        }
    }

    #[test]
    fn empty_pool_returns_do_nothing() {
        let optimizer = PortfolioOptimizer::default();
        let result = optimizer.select(vec![]);
        assert!(result.do_nothing);
        assert!(result.selected.is_empty());
    }

    #[test]
    fn single_candidate_is_selected() {
        let optimizer = PortfolioOptimizer::default();
        let result = optimizer.select(vec![make_candidate("a", "x", 5.0, "aud1")]);
        assert!(!result.do_nothing);
        assert_eq!(result.selected.len(), 1);
        assert!((result.total_expected_fans - 5.0).abs() < 0.01);
    }

    #[test]
    fn higher_expected_fans_selected_first() {
        let optimizer = PortfolioOptimizer::default();
        let result = optimizer.select(vec![
            make_candidate("a", "x", 2.0, "aud1"),
            make_candidate("b", "y", 10.0, "aud2"),
        ]);
        assert_eq!(result.selected[0].opportunity_id.template_id, "b");
    }

    #[test]
    fn budget_limit_caps_selection() {
        let config = PortfolioConfig {
            max_dispatches: 2,
            ..Default::default()
        };
        let optimizer = PortfolioOptimizer::new(config);
        let result = optimizer.select(vec![
            make_candidate("a", "x", 5.0, "aud1"),
            make_candidate("b", "y", 4.0, "aud2"),
            make_candidate("c", "z", 3.0, "aud3"),
        ]);
        assert_eq!(result.selected.len(), 2);
        assert_eq!(result.rejected.len(), 1);
        assert_eq!(
            result.rejected[0].reason,
            RejectionReason::MaxDispatchesReached
        );
    }

    #[test]
    fn audience_overlap_reduces_marginal_value() {
        let config = PortfolioConfig {
            max_dispatches: 5,
            audience_overlap_penalty: 0.5,
            fatigue_decay: 1.0, // disable fatigue to isolate overlap
            min_marginal_value: 0.01,
            ..Default::default()
        };
        let optimizer = PortfolioOptimizer::new(config);
        // Two candidates targeting the same audience.
        let result = optimizer.select(vec![
            make_candidate("a", "x", 10.0, "same_audience"),
            make_candidate("b", "y", 8.0, "same_audience"),
        ]);
        // First candidate: marginal = 10.0 (no overlap).
        // Second candidate: marginal = 8.0 * (1 - 0.5 * 1) * 1.0 = 4.0.
        // Both should be selected (4.0 > min_marginal_value).
        assert_eq!(result.selected.len(), 2);
        // Total = 10.0 + 4.0 = 14.0
        assert!((result.total_expected_fans - 14.0).abs() < 0.01);
    }

    #[test]
    fn high_overlap_penalty_rejects_second_candidate() {
        let config = PortfolioConfig {
            max_dispatches: 5,
            audience_overlap_penalty: 1.0,
            fatigue_decay: 1.0, // disable fatigue to isolate overlap
            min_marginal_value: 0.5,
            ..Default::default()
        };
        let optimizer = PortfolioOptimizer::new(config);
        let result = optimizer.select(vec![
            make_candidate("a", "x", 10.0, "same_audience"),
            make_candidate("b", "y", 8.0, "same_audience"),
        ]);
        // First: marginal = 10.0. Second: marginal = 8.0 * 0.0 * 1.0 = 0.0 < 0.5.
        assert_eq!(result.selected.len(), 1);
        assert_eq!(result.rejected.len(), 1);
    }

    #[test]
    fn different_audiences_have_no_overlap_penalty() {
        let optimizer = PortfolioOptimizer::default();
        let result = optimizer.select(vec![
            make_candidate("a", "x", 5.0, "aud1"),
            make_candidate("b", "y", 5.0, "aud2"),
        ]);
        assert_eq!(result.selected.len(), 2);
        // No overlap: total = 5.0 + 5.0 = 10.0
        assert!((result.total_expected_fans - 10.0).abs() < 0.01);
    }

    #[test]
    fn negative_marginal_value_triggers_do_nothing() {
        let config = PortfolioConfig {
            min_marginal_value: 1.0,
            ..Default::default()
        };
        let optimizer = PortfolioOptimizer::new(config);
        let result = optimizer.select(vec![make_candidate("a", "x", 0.5, "aud1")]);
        assert!(result.do_nothing);
        assert!(result.selected.is_empty());
    }

    #[test]
    fn greedy_picks_highest_marginal_not_highest_absolute() {
        let config = PortfolioConfig {
            max_dispatches: 2,
            audience_overlap_penalty: 0.5,
            fatigue_decay: 1.0, // disable fatigue to isolate overlap
            min_marginal_value: 0.01,
            ..Default::default()
        };
        let optimizer = PortfolioOptimizer::new(config);
        // Candidate A: 10 fans, audience "shared"
        // Candidate B: 8 fans, audience "unique"
        // Candidate C: 9 fans, audience "shared"
        //
        // Greedy step 1: pick A (10.0 marginal).
        // Greedy step 2: B has marginal 8.0 (no overlap), C has marginal
        //   9.0 * (1 - 0.5) * 1.0 = 4.5 (overlap with A). Pick B (8.0 > 4.5).
        let result = optimizer.select(vec![
            make_candidate("a", "x", 10.0, "shared"),
            make_candidate("b", "y", 8.0, "unique"),
            make_candidate("c", "z", 9.0, "shared"),
        ]);
        assert_eq!(result.selected.len(), 2);
        assert_eq!(result.selected[0].opportunity_id.template_id, "a");
        assert_eq!(result.selected[1].opportunity_id.template_id, "b");
    }

    #[test]
    fn total_expected_fans_uses_marginal_not_absolute() {
        let config = PortfolioConfig {
            max_dispatches: 5,
            audience_overlap_penalty: 0.3,
            fatigue_decay: 1.0, // disable fatigue to isolate overlap
            min_marginal_value: 0.01,
            ..Default::default()
        };
        let optimizer = PortfolioOptimizer::new(config);
        let result = optimizer.select(vec![
            make_candidate("a", "x", 10.0, "shared"),
            make_candidate("b", "y", 8.0, "shared"),
        ]);
        // Total = 10.0 + 8.0 * (1 - 0.3) * 1.0 = 10.0 + 5.6 = 15.6
        assert!((result.total_expected_fans - 15.6).abs() < 0.01);
    }

    #[test]
    fn portfolio_config_defaults_are_sensible() {
        let config = PortfolioConfig::default();
        assert_eq!(config.max_dispatches, 5);
        assert_eq!(config.cost_budget, 0);
        assert!((config.audience_overlap_penalty - 0.3).abs() < 0.01);
        assert!((config.fatigue_decay - 0.9).abs() < 0.01);
        assert!((config.min_marginal_value - 0.1).abs() < 0.01);
    }

    #[test]
    fn cost_budget_limits_selection() {
        let config = PortfolioConfig {
            max_dispatches: 10,
            cost_budget: 3, // only 3 cost units
            ..Default::default()
        };
        let optimizer = PortfolioOptimizer::new(config);
        // Each candidate costs 1.
        let result = optimizer.select(vec![
            make_candidate("a", "x", 5.0, "aud1"),
            make_candidate("b", "y", 4.0, "aud2"),
            make_candidate("c", "z", 3.0, "aud3"),
            make_candidate("d", "w", 2.0, "aud4"),
        ]);
        // Should select 3 (cost budget = 3, each costs 1).
        assert_eq!(result.selected.len(), 3);
        assert_eq!(result.rejected.len(), 1);
        assert_eq!(result.rejected[0].reason, RejectionReason::BudgetExhausted);
    }

    #[test]
    fn cost_budget_with_variable_costs() {
        let config = PortfolioConfig {
            max_dispatches: 10,
            cost_budget: 5,
            min_marginal_value: 0.01,
            ..Default::default()
        };
        let optimizer = PortfolioOptimizer::new(config);
        let mut expensive = make_candidate("a", "x", 10.0, "aud1");
        expensive.cost = 3;
        let mut cheap1 = make_candidate("b", "y", 4.0, "aud2");
        cheap1.cost = 1;
        let mut cheap2 = make_candidate("c", "z", 3.0, "aud3");
        cheap2.cost = 1;
        let mut cheap3 = make_candidate("d", "w", 2.0, "aud4");
        cheap3.cost = 1;
        let result = optimizer.select(vec![expensive, cheap1, cheap2, cheap3]);
        // expensive (cost 3) + cheap1 (cost 1) + cheap2 (cost 1) = cost 5.
        // cheap3 would exceed budget.
        assert_eq!(result.selected.len(), 3);
    }

    #[test]
    fn fatigue_reduces_repeated_dispatches() {
        let config = PortfolioConfig {
            max_dispatches: 5,
            cost_budget: 0,
            audience_overlap_penalty: 0.0, // disable overlap to isolate fatigue
            fatigue_decay: 0.5,            // 50% reduction per additional dispatch
            min_marginal_value: 1.0,
        };
        let optimizer = PortfolioOptimizer::new(config);
        // Three candidates to the same audience, all with 10 fans.
        let result = optimizer.select(vec![
            make_candidate("a", "x", 10.0, "same"),
            make_candidate("b", "y", 10.0, "same"),
            make_candidate("c", "z", 10.0, "same"),
        ]);
        // First: marginal = 10.0 (no fatigue).
        // Second: marginal = 10.0 * 0.5 = 5.0 (> 1.0, selected).
        // Third: marginal = 10.0 * 0.25 = 2.5 (> 1.0, selected).
        assert_eq!(result.selected.len(), 3);
        // Total = 10.0 + 5.0 + 2.5 = 17.5
        assert!((result.total_expected_fans - 17.5).abs() < 0.01);
    }

    #[test]
    fn fatigue_combined_with_overlap() {
        let config = PortfolioConfig {
            max_dispatches: 5,
            cost_budget: 0,
            audience_overlap_penalty: 0.3,
            fatigue_decay: 0.8,
            min_marginal_value: 0.5,
        };
        let optimizer = PortfolioOptimizer::new(config);
        let result = optimizer.select(vec![
            make_candidate("a", "x", 10.0, "same"),
            make_candidate("b", "y", 10.0, "same"),
        ]);
        // First: marginal = 10.0 * 1.0 * 1.0 = 10.0
        // Second: marginal = 10.0 * (1 - 0.3) * 0.8 = 10.0 * 0.7 * 0.8 = 5.6
        assert_eq!(result.selected.len(), 2);
        assert!((result.total_expected_fans - 15.6).abs() < 0.01);
    }

    #[test]
    fn cost_budget_zero_uses_max_dispatches_only() {
        let config = PortfolioConfig {
            max_dispatches: 2,
            cost_budget: 0, // disabled
            ..Default::default()
        };
        let optimizer = PortfolioOptimizer::new(config);
        let result = optimizer.select(vec![
            make_candidate("a", "x", 5.0, "aud1"),
            make_candidate("b", "y", 4.0, "aud2"),
            make_candidate("c", "z", 3.0, "aud3"),
        ]);
        // cost_budget=0 means disabled → max_dispatches=2 is the limit.
        assert_eq!(result.selected.len(), 2);
    }
}
