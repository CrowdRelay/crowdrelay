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

use crate::decision_value::DecisionValue;
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
///
/// This is a **thin wrapper** around `DecisionValue` — the canonical
/// intrinsic value object. The optimizer reads `decision_value` for all
/// value semantics. Identity/routing fields stay outside DecisionValue
/// because they are not value semantics.
///
/// `DecisionValue` = intrinsic value before portfolio interactions.
/// `PortfolioOptimizer` = marginal value after overlap, fatigue, budget.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortfolioCandidate {
    // ── Identity / routing (not value semantics) ──
    /// The opportunity identity (stable across cycles).
    pub opportunity_id: OpportunityId,
    /// The audience key — candidates with the same audience key overlap.
    /// E.g. "subreddit:r_MetalMusic" or "venue:Warsaw_Palladium".
    pub audience_key: String,
    /// The context that produced this candidate (for tracing).
    pub source_context: String,
    /// The action payload key (for linking to the persist layer).
    pub action_key: String,
    /// The EFE score (lower = better). Kept for backward compat and
    /// logging only — the optimizer uses `decision_value.total()`.
    pub efe_score: f64,

    // ── Canonical value ──
    /// The intrinsic decision value — one source of truth for all value
    /// semantics. The optimizer computes marginal value from this.
    pub decision_value: DecisionValue,
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
    /// When `do_nothing` is true, the economic rationale for WAIT.
    /// Explains why waiting produces more expected Y30 fan value than
    /// dispatching any available candidate.
    pub wait_reason: Option<String>,
}

/// The value of WAIT (doing nothing) expressed in expected incremental
/// Y30 fans. Every term is in the **same fan-value utility space** —
/// no mixed-unit scalar soup.
///
/// WAIT does NOT become more valuable merely because many expensive
/// candidates exist. `avoided_cost` is NOT a term — resource cost is
/// already captured in the action's value. WAIT's value comes from
/// information, fatigue recovery, and option value, minus the
/// opportunity cost of not acting.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct WaitCandidateValue {
    /// Value of information from pending measurements, expressed
    /// in expected incremental Y30 fans. Computed as:
    ///   VOI = count_pending * avg_treatment_std * DECISION_SENSITIVITY
    /// where DECISION_SENSITIVITY converts uncertainty to expected
    /// fan value — calibrated empirically later.
    pub value_of_information: f64,
    /// Fatigue recovery value in expected Y30 fans. Computed as:
    ///   sum(audience_fatigue * fatigue_recovery_per_cycle * expected_fans)
    /// This is in fan-value space because it multiplies fatigue count
    /// by the expected fan value of a recovered audience.
    pub fatigue_recovery_value: f64,
    /// Opportunity cost of NOT acting now. This is NEGATIVE.
    ///   -best_candidate_expected_y30
    /// Waiting costs the fan value we could have gained now.
    pub opportunity_cost: f64,
    /// Preserved option value — placeholder, 0.0 for now.
    /// Future: V(wait) = E[best_future_action] - immediate_action_value
    pub option_value: f64,
}

impl WaitCandidateValue {
    /// The total WAIT utility — sum of all components.
    /// Every term is in expected incremental Y30 fans.
    #[must_use]
    pub fn total(&self) -> f64 {
        self.value_of_information
            + self.fatigue_recovery_value
            + self.option_value
            + self.opportunity_cost
    }

    /// The decision sensitivity constant — converts treatment uncertainty
    /// to expected fan value. Conservative: 0.1 means VOI is small relative
    /// to typical fan values (2-5). Monitor in production — if WAIT never
    /// wins, increase; if it always wins, decrease.
    const DECISION_SENSITIVITY: f64 = 0.1;

    /// Computes the WAIT candidate value from the current state.
    ///
    /// - `best_candidate_expected_y30`: the highest expected Y30 among
    ///   available action candidates (0.0 if no candidates).
    /// - `count_pending_measurements`: number of measurements whose
    ///   outcomes haven't been observed yet.
    /// - `avg_treatment_std`: average treatment-effect std across
    ///   pending candidates.
    /// - `fatigue_recovery_value`: pre-computed fatigue recovery in
    ///   fan-value space (sum of audience fatigue × recovery × expected
    ///   fans per audience).
    #[must_use]
    pub fn compute(
        best_candidate_expected_y30: f64,
        count_pending_measurements: u32,
        avg_treatment_std: f64,
        fatigue_recovery_value: f64,
    ) -> Self {
        Self {
            value_of_information: f64::from(count_pending_measurements)
                * avg_treatment_std
                * Self::DECISION_SENSITIVITY,
            fatigue_recovery_value,
            opportunity_cost: -best_candidate_expected_y30,
            option_value: 0.0,
        }
    }
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
    /// The maximum total resource cost per cycle. Candidates have a
    /// `resource_cost` field; the optimizer stops when the total cost of
    /// selected candidates exceeds this budget. Set to 0.0 to disable cost
    /// budgeting (use only `max_dispatches`).
    pub cost_budget: f64,
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
            cost_budget: 0.0, // 0 = disabled (use max_dispatches only)
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
    /// 1. Sort candidates by intrinsic `decision_value.total()`.
    /// 2. At each step, pick the candidate with the highest marginal value
    ///    (intrinsic total × overlap × fatigue × bridge penalty).
    /// 3. Stop when marginal value < min_marginal_value, dispatch count
    ///    budget exhausted, or cost budget exhausted.
    ///
    /// # North Star objective
    ///
    /// The optimizer uses `decision_value.total()` as the ranking signal.
    /// DecisionValue carries the estimation regime (Y30Direct, Y14Bridged,
    /// or OutcomeModel) and full provenance — the optimizer applies a
    /// bridge reliability penalty for uncalibrated Y14Bridged candidates.
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
        let mut total_cost: f64 = 0.0;
        // Sort by intrinsic decision value (total) — highest first.
        remaining.sort_by(|a, b| {
            b.decision_value
                .total()
                .partial_cmp(&a.decision_value.total())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut total_expected_fans = 0.0;
        while selected.len() < self.config.max_dispatches as usize && !remaining.is_empty() {
            // Find the candidate with the highest marginal value.
            let mut best_idx = None;
            let mut best_marginal = f64::NEG_INFINITY;
            for (i, candidate) in remaining.iter().enumerate() {
                // Check cost budget — skip candidates that would exceed it.
                if self.config.cost_budget > 0.0
                    && total_cost + candidate.decision_value.resource_cost.units
                        > self.config.cost_budget
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
                // Marginal value = intrinsic total × portfolio interactions.
                // DecisionValue.total() is the intrinsic value; the optimizer
                // applies overlap and fatigue as multiplicative modifiers.
                let intrinsic = candidate.decision_value.total();
                // Bridge reliability penalty: when the bridge is unreliable
                // and the regime is Y14Bridged, apply a confidence penalty.
                // The bridge is a temporary belief, not a semi-factual
                // substitute for Y30.
                let bridge_penalty = if !candidate.decision_value.bridge_is_reliable
                    && candidate.decision_value.estimation_regime
                        == crate::decision_value::EstimationRegime::Y14Bridged
                {
                    0.8 // 20% penalty for uncalibrated bridge
                } else {
                    1.0
                };
                let marginal =
                    intrinsic * overlap_factor.max(0.0) * fatigue_factor * bridge_penalty;
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
                total_cost += candidate.decision_value.resource_cost.units;
                *audience_counts
                    .entry(candidate.audience_key.clone())
                    .or_insert(0) += 1;
                total_expected_fans += best_marginal;
                // Compute the decision mode from DecisionValue provenance:
                // - Learn: low sample size → information gain
                // - Explore: high uncertainty relative to expected value
                // - Exploit: high confidence, high expected value
                let dv = &candidate.decision_value;
                let mode = if dv.sample_size < 10 && dv.uncertainty > 0.0 {
                    DecisionMode::Learn
                } else if dv.uncertainty > dv.expected_incremental_y30.abs().max(1.0) {
                    DecisionMode::Explore
                } else {
                    DecisionMode::Exploit
                };
                let mut candidate = candidate;
                candidate.decision_value.decision_mode = mode;
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
            wait_reason: None,
        }
    }

    /// Selects the optimal portfolio, comparing action candidates against
    /// a WAIT candidate. If WAIT has higher total value than the best
    /// action's marginal value, the brain does nothing with an economic
    /// rationale.
    ///
    /// See [`WaitCandidateValue::compute`] for the WAIT value computation.
    #[must_use]
    pub fn select_with_wait(
        &self,
        candidates: Vec<PortfolioCandidate>,
        wait: WaitCandidateValue,
    ) -> PortfolioSelection {
        if candidates.is_empty() {
            let wait_total = wait.total();
            return PortfolioSelection {
                do_nothing: true,
                wait_reason: if wait_total > 0.0 {
                    Some(format!(
                        "WAIT wins: no action candidates. VOI={:.2}, fatigue_recovery={:.2}, \
                         opportunity_cost={:.2}, total={:.2}",
                        wait.value_of_information,
                        wait.fatigue_recovery_value,
                        wait.opportunity_cost,
                        wait_total,
                    ))
                } else {
                    None
                },
                ..Default::default()
            };
        }
        // Compute the best candidate's intrinsic value (before overlap/fatigue).
        let best_action_value = candidates
            .iter()
            .map(|c| c.decision_value.total())
            .fold(0.0_f64, f64::max);
        let wait_total = wait.total();
        // WAIT wins when its net utility is positive — NOT when it exceeds
        // the best action value. The opportunity_cost term in WaitCandidateValue
        // already subtracts the best action's Y30, so:
        //
        //   wait_total = VOI + fatigue + option - best_action_y30
        //
        // If wait_total > 0, then VOI + fatigue + option > best_action_y30,
        // meaning the value of waiting exceeds the value of acting.
        //
        // The old code compared wait_total > best_y30, which double-counted
        // the opportunity cost. This is the corrected math.
        if wait_total > 0.0 && wait_total > self.config.min_marginal_value {
            let reason = format!(
                "WAIT wins: VOI={:.2}, fatigue_recovery={:.2}, option_value={:.2}, \
                 opportunity_cost={:.2}, net_utility={:.2} > 0 (best_action_value={:.2})",
                wait.value_of_information,
                wait.fatigue_recovery_value,
                wait.option_value,
                wait.opportunity_cost,
                wait_total,
                best_action_value,
            );
            let rejected: Vec<PortfolioRejection> = candidates
                .into_iter()
                .map(|c| PortfolioRejection {
                    opportunity_key: c.opportunity_id.to_string(),
                    reason: RejectionReason::NegativeMarginalValue,
                })
                .collect();
            return PortfolioSelection {
                selected: Vec::new(),
                rejected,
                total_expected_fans: 0.0,
                do_nothing: true,
                wait_reason: Some(reason),
            };
        }
        // WAIT doesn't win — proceed with normal selection.
        self.select(candidates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::causal_model::DispatchContext;
    use crate::opportunity::OpportunityAction;
    use crate::resource_cost::ResourceCost;

    fn make_candidate(
        template: &str,
        target: &str,
        expected_fans: f64,
        audience: &str,
    ) -> PortfolioCandidate {
        let ctx = DispatchContext::default();
        let decision_value = DecisionValue {
            expected_incremental_y30: expected_fans,
            uncertainty: 0.0,
            p_meaningful_effect: 0.0,
            estimation_regime: crate::decision_value::EstimationRegime::OutcomeModel,
            evidence_quality: crate::evidence::EvidenceQuality::Observational,
            sample_size: 0,
            uses_y30: false,
            bridge_confidence: 0,
            bridge_is_reliable: false,
            resource_cost: ResourceCost::configured(1.0),
            pragmatic_value: expected_fans,
            epistemic_value: 0.0,
            exploration_value: 0.0,
            risk_penalty: 0.0,
            opportunity_cost: 0.0,
            decision_mode: DecisionMode::Exploit,
        };
        PortfolioCandidate {
            opportunity_id: OpportunityId::new(template, target, OpportunityAction::Post, &ctx),
            efe_score: -expected_fans,
            audience_key: audience.to_owned(),
            source_context: "GrowthIntelligence".to_owned(),
            action_key: format!("action:{template}:{target}"),
            decision_value,
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
        assert_eq!(config.cost_budget, 0.0);
        assert!((config.audience_overlap_penalty - 0.3).abs() < 0.01);
        assert!((config.fatigue_decay - 0.9).abs() < 0.01);
        assert!((config.min_marginal_value - 0.1).abs() < 0.01);
    }

    #[test]
    fn cost_budget_limits_selection() {
        let config = PortfolioConfig {
            max_dispatches: 10,
            cost_budget: 3.0, // only 3 cost units
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
            cost_budget: 5.0,
            min_marginal_value: 0.01,
            ..Default::default()
        };
        let optimizer = PortfolioOptimizer::new(config);
        let mut expensive = make_candidate("a", "x", 10.0, "aud1");
        expensive.decision_value.resource_cost = ResourceCost::configured(3.0);
        let mut cheap1 = make_candidate("b", "y", 4.0, "aud2");
        cheap1.decision_value.resource_cost = ResourceCost::configured(1.0);
        let mut cheap2 = make_candidate("c", "z", 3.0, "aud3");
        cheap2.decision_value.resource_cost = ResourceCost::configured(1.0);
        let mut cheap3 = make_candidate("d", "w", 2.0, "aud4");
        cheap3.decision_value.resource_cost = ResourceCost::configured(1.0);
        let result = optimizer.select(vec![expensive, cheap1, cheap2, cheap3]);
        // expensive (cost 3) + cheap1 (cost 1) + cheap2 (cost 1) = cost 5.
        // cheap3 would exceed budget.
        assert_eq!(result.selected.len(), 3);
    }

    #[test]
    fn fatigue_reduces_repeated_dispatches() {
        let config = PortfolioConfig {
            max_dispatches: 5,
            cost_budget: 0.0,
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
            cost_budget: 0.0,
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
            cost_budget: 0.0, // disabled
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
