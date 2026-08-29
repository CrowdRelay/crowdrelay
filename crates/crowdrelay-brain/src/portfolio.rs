//! Portfolio optimizer — global candidate pool with submodular greedy selection.
//!
//! The brain doesn't dispatch candidates context-by-context — it collects ALL
//! candidates from ALL contexts into a global pool and selects the optimal
//! portfolio. This is the "GET FANS" optimizer: it maximizes expected
//! incremental fans across the entire action space, subject to budget
//! constraints and audience overlap.
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
//! # Algorithm: submodular greedy
//!
//! Fan acquisition has diminishing returns (posting to the same subreddit
//! twice in one day doesn't double your fans). This makes the objective
//! function submodular, and greedy selection is guaranteed to achieve
//! (1 - 1/e) ≈ 63% of the optimal. The algorithm:
//!
//! 1. Start with an empty portfolio.
//! 2. At each step, add the candidate with the highest marginal value
//!    (expected fans minus overlap penalty with already-selected candidates).
//! 3. Stop when the marginal value drops below zero or the budget is exhausted.
//! 4. "DO NOTHING" is always a candidate — if all marginal values are negative,
//!    the brain does nothing.

use serde::Serialize;

use crate::opportunity::OpportunityId;

/// A candidate in the global pool, scored and ready for portfolio selection.
#[derive(Clone, Debug, Serialize)]
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
}

/// The result of portfolio optimization.
#[derive(Clone, Debug, Default, Serialize)]
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
#[derive(Clone, Debug, Serialize)]
pub struct PortfolioRejection {
    pub opportunity_key: String,
    pub reason: RejectionReason,
}

/// Why a candidate was not selected for the portfolio.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionReason {
    /// The marginal value was negative (not worth dispatching).
    NegativeMarginalValue,
    /// The budget was exhausted.
    BudgetExhausted,
    /// The audience overlap penalty made this candidate not worth it.
    AudienceOverlap,
    /// The candidate was superseded by a better candidate for the same audience.
    Superseded,
}

/// Configuration for the portfolio optimizer.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct PortfolioConfig {
    /// The maximum number of dispatches per cycle.
    pub max_dispatches: u32,
    /// The audience overlap penalty: each additional candidate targeting the
    /// same audience gets its expected fans multiplied by (1 - penalty × count).
    /// 0.0 = no penalty, 1.0 = full penalty (second dispatch to same audience
    /// has zero value).
    pub audience_overlap_penalty: f64,
    /// The minimum marginal value required to add a candidate to the portfolio.
    /// Below this, the brain prefers DO NOTHING.
    pub min_marginal_value: f64,
}

impl Default for PortfolioConfig {
    fn default() -> Self {
        Self {
            max_dispatches: 5,
            audience_overlap_penalty: 0.3,
            min_marginal_value: 0.1,
        }
    }
}

/// The portfolio optimizer — selects the optimal set of candidates from the
/// global pool.
#[derive(Clone, Debug, Default, Serialize)]
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
    /// Uses submodular greedy selection:
    /// 1. Sort candidates by expected fans (descending).
    /// 2. At each step, pick the candidate with the highest marginal value
    ///    (expected fans minus audience overlap with already-selected).
    /// 3. Stop when marginal value < min_marginal_value or budget exhausted.
    #[must_use]
    pub fn select(&self, candidates: Vec<PortfolioCandidate>) -> PortfolioSelection {
        if candidates.is_empty() {
            return PortfolioSelection {
                do_nothing: true,
                ..Default::default()
            };
        }
        // Track audience usage for overlap penalty computation.
        let mut audience_counts: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();
        let mut selected: Vec<PortfolioCandidate> = Vec::new();
        let mut rejected: Vec<PortfolioRejection> = Vec::new();
        let mut remaining: Vec<PortfolioCandidate> = candidates;
        // Sort by expected fans descending — greedy starts with the best.
        remaining.sort_by(|a, b| {
            b.expected_fans
                .partial_cmp(&a.expected_fans)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut total_expected_fans = 0.0;
        while selected.len() < self.config.max_dispatches as usize && !remaining.is_empty() {
            // Find the candidate with the highest marginal value.
            let mut best_idx = None;
            let mut best_marginal = f64::NEG_INFINITY;
            for (i, candidate) in remaining.iter().enumerate() {
                let audience_count = audience_counts
                    .get(&candidate.audience_key)
                    .copied()
                    .unwrap_or(0);
                let overlap_factor =
                    1.0 - self.config.audience_overlap_penalty * audience_count as f64;
                let marginal = candidate.expected_fans * overlap_factor.max(0.0);
                if marginal > best_marginal {
                    best_marginal = marginal;
                    best_idx = Some(i);
                }
            }
            // Check stopping conditions.
            if best_marginal < self.config.min_marginal_value {
                // All remaining candidates have negative or low marginal value.
                // Reject the rest and stop.
                for candidate in remaining.drain(..) {
                    rejected.push(PortfolioRejection {
                        opportunity_key: candidate.opportunity_id.to_string(),
                        reason: if best_marginal < 0.0 {
                            RejectionReason::NegativeMarginalValue
                        } else {
                            RejectionReason::AudienceOverlap
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
                *audience_counts
                    .entry(candidate.audience_key.clone())
                    .or_insert(0) += 1;
                total_expected_fans += best_marginal;
                selected.push(candidate);
            } else {
                break;
            }
        }
        // Reject any remaining candidates.
        for candidate in remaining {
            rejected.push(PortfolioRejection {
                opportunity_key: candidate.opportunity_id.to_string(),
                reason: RejectionReason::BudgetExhausted,
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
        assert_eq!(result.rejected[0].reason, RejectionReason::BudgetExhausted);
    }

    #[test]
    fn audience_overlap_reduces_marginal_value() {
        let config = PortfolioConfig {
            max_dispatches: 5,
            audience_overlap_penalty: 0.5,
            min_marginal_value: 0.01,
        };
        let optimizer = PortfolioOptimizer::new(config);
        // Two candidates targeting the same audience.
        let result = optimizer.select(vec![
            make_candidate("a", "x", 10.0, "same_audience"),
            make_candidate("b", "y", 8.0, "same_audience"),
        ]);
        // First candidate: marginal = 10.0 (no overlap).
        // Second candidate: marginal = 8.0 * (1 - 0.5 * 1) = 4.0.
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
            min_marginal_value: 0.5,
        };
        let optimizer = PortfolioOptimizer::new(config);
        let result = optimizer.select(vec![
            make_candidate("a", "x", 10.0, "same_audience"),
            make_candidate("b", "y", 8.0, "same_audience"),
        ]);
        // First: marginal = 10.0. Second: marginal = 8.0 * 0.0 = 0.0 < 0.5.
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
            min_marginal_value: 0.01,
        };
        let optimizer = PortfolioOptimizer::new(config);
        // Candidate A: 10 fans, audience "shared"
        // Candidate B: 8 fans, audience "unique"
        // Candidate C: 9 fans, audience "shared"
        //
        // Greedy step 1: pick A (10.0 marginal).
        // Greedy step 2: B has marginal 8.0 (no overlap), C has marginal
        //   9.0 * (1 - 0.5) = 4.5 (overlap with A). Pick B (8.0 > 4.5).
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
            min_marginal_value: 0.01,
        };
        let optimizer = PortfolioOptimizer::new(config);
        let result = optimizer.select(vec![
            make_candidate("a", "x", 10.0, "shared"),
            make_candidate("b", "y", 8.0, "shared"),
        ]);
        // Total = 10.0 + 8.0 * (1 - 0.3) = 10.0 + 5.6 = 15.6
        assert!((result.total_expected_fans - 15.6).abs() < 0.01);
    }

    #[test]
    fn portfolio_config_defaults_are_sensible() {
        let config = PortfolioConfig::default();
        assert_eq!(config.max_dispatches, 5);
        assert!((config.audience_overlap_penalty - 0.3).abs() < 0.01);
        assert!((config.min_marginal_value - 0.1).abs() < 0.01);
    }
}
