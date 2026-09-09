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

mod adjustments;
mod selection;
mod wait_value;
pub use adjustments::{AdjustmentInputs, MarginalAdjustments};
pub use selection::{PortfolioRejection, PortfolioSelection, RejectionReason};
pub use wait_value::WaitCandidateValue;

use crate::decision_value::DecisionValue;
use crate::opportunity::OpportunityId;

/// Non-economic candidate-generation / exploration signal from EFE.
///
/// This is explicitly NOT part of the candidate's economic value. EFE
/// decides what is worth *learning about* (candidate generation,
/// exploration allocation). DecisionValue decides what is worth *doing*
/// (portfolio ranking). The optimizer must never combine `EfeSignal`
/// with `DecisionValue.total()`.
///
/// Carried on `PortfolioCandidate::generation_signal` for audit and
/// exploration provenance — the optimizer ignores it for ranking.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct EfeSignal {
    /// Information gain — how much the brain would learn from this
    /// action's outcome. From EFE's epistemic term.
    pub information_gain: f64,
    /// Novelty — how unexplored this (template, context) pair is.
    /// From the exploration memory.
    pub novelty: f64,
    /// The raw EFE score (lower = better). Kept for logging and
    /// candidate-generation provenance only.
    pub efe_score: f64,
}

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
    /// Non-economic candidate-generation / exploration signal from EFE.
    /// The optimizer IGNORES this for ranking — it is carried for audit
    /// and exploration provenance only. EFE decides what is worth
    /// learning about; DecisionValue decides what is worth doing.
    pub generation_signal: Option<EfeSignal>,
    /// P0-2: Whether this candidate is a treatment assignment in an active
    /// experiment. Experimental candidates can use the
    /// `experimental_dispatch_budget` (additional slots beyond
    /// `max_dispatches`) when the value of information justifies the spend.
    /// Control assignments consume zero experimental slots.
    #[serde(default)]
    pub is_experimental: bool,

    // ── Canonical value ──
    /// The intrinsic decision value — one source of truth for all value
    /// semantics. The optimizer computes marginal value from this.
    pub decision_value: DecisionValue,
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
    /// P0-2: Additional treatment dispatch slots for active experiments.
    /// These slots are ONLY consumed by candidates with
    /// `is_experimental=true`. Control assignments consume zero
    /// experimental slots. The budget is NOT spent blindly — experimental
    /// candidates must still clear `min_marginal_value` and the optimizer's
    /// safety/value gates. Default 0 (disabled).
    #[serde(default)]
    pub experimental_dispatch_budget: u32,
    /// Minimum dispatches per cycle when candidates with positive value
    /// exist. This breaks the WAIT deadlock: when the brain has many
    /// pending measurements, VOI can exceed the best action's value, making
    /// WAIT win every cycle. But measurements only resolve after the brain
    /// acts and the measurement window elapses — so WAIT creates a
    /// cold-start deadlock. `min_dispatches=1` guarantees the brain always
    /// tries at least one candidate with positive value, even if WAIT's
    /// net utility is positive. WAIT still blocks candidates whose
    /// individual value is below the WAIT threshold.
    #[serde(default = "default_min_dispatches")]
    pub min_dispatches: u32,
}

fn default_min_dispatches() -> u32 {
    1
}

impl Default for PortfolioConfig {
    fn default() -> Self {
        Self {
            max_dispatches: 5,
            cost_budget: 0.0, // 0 = disabled (use max_dispatches only)
            audience_overlap_penalty: 0.3,
            fatigue_decay: 0.9,
            min_marginal_value: 0.1,
            experimental_dispatch_budget: 0,
            min_dispatches: 1,
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
        let mut marginal_adjustments: std::collections::BTreeMap<String, MarginalAdjustments> =
            std::collections::BTreeMap::new();
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
        let mut experimental_slots_used: u32 = 0;
        // P0-2: The total slot limit is max_dispatches + experimental_dispatch_budget.
        // Experimental slots are only available to candidates with is_experimental=true.
        // Control assignments consume zero experimental slots (they are not dispatched).
        let total_slot_limit =
            (self.config.max_dispatches + self.config.experimental_dispatch_budget) as usize;
        while selected.len() < total_slot_limit && !remaining.is_empty() {
            // Find the candidate with the highest marginal value.
            let mut best_idx = None;
            let mut best_marginal = f64::NEG_INFINITY;
            let mut best_adjustments = MarginalAdjustments::default();
            for (i, candidate) in remaining.iter().enumerate() {
                // Check cost budget — skip candidates that would exceed it.
                if self.config.cost_budget > 0.0
                    && total_cost + candidate.decision_value.resource_cost.units
                        > self.config.cost_budget
                {
                    continue;
                }
                // P0-2: If normal budget is exhausted, only experimental
                // candidates can use the remaining experimental slots.
                let normal_budget_exhausted = selected.len() >= self.config.max_dispatches as usize;
                if normal_budget_exhausted {
                    if !candidate.is_experimental {
                        continue; // Non-experimental candidate, no slots left.
                    }
                    if experimental_slots_used >= self.config.experimental_dispatch_budget {
                        continue; // Experimental budget also exhausted.
                    }
                }
                let audience_count = audience_counts
                    .get(&candidate.audience_key)
                    .copied()
                    .unwrap_or(0);
                // Marginal value = intrinsic total × portfolio interactions.
                // DecisionValue.total() is the intrinsic value; the optimizer
                // applies overlap and fatigue as multiplicative modifiers.
                let intrinsic = candidate.decision_value.total();
                // Bridge reliability penalty: a `Y14Bridged` estimate whose
                // bridge is not yet calibrated is a Y14 number wearing a Y30
                // label, so it is discounted here rather than trusted at face
                // value.
                //
                // Be exact about what this is, because it is the one place an
                // epistemic fact reaches economic ranking. `0.8` is hand-tuned:
                // nothing measured it, and it is not a probability, a variance
                // or a fan count. It survives the `DecisionValue` invariant
                // only because it is applied to the **marginal**, alongside
                // overlap and fatigue, and never to the intrinsic value —
                // `total()` stays a sum of commensurate fan-equivalent terms
                // and a reader can still see what a candidate is worth before
                // the portfolio touched it.
                //
                // That is a defensible place for it and not a free pass. A
                // multiplier here reorders candidates exactly as one inside
                // `total()` would; the difference is that this one is visible
                // as a portfolio interaction rather than hidden in the
                // candidate's own value. If more of these accumulate, the
                // marginal becomes the weighted soup `total()` refuses, one
                // defensible coefficient at a time.
                let bridge_penalty = if !candidate.decision_value.bridge_is_reliable
                    && candidate.decision_value.estimation_regime
                        == crate::decision_value::EstimationRegime::Y14Bridged
                {
                    0.8 // 20% penalty for uncalibrated bridge
                } else {
                    1.0
                };
                // Through the ledger rather than as a bare product, so the
                // answer to "why 7.2 and not 10.0" is recorded rather than
                // reconstructed. Same arithmetic, same order — and the inputs
                // travel with the deltas, because the audience count is
                // portfolio state that vanishes with the cycle and the two
                // coefficients are configuration a reader would otherwise have
                // to take from a config that has since been edited.
                let adjustments = MarginalAdjustments::apply(
                    intrinsic,
                    AdjustmentInputs {
                        audience_count,
                        overlap_penalty: self.config.audience_overlap_penalty,
                        fatigue_decay: self.config.fatigue_decay,
                        bridge_factor: bridge_penalty,
                    },
                );
                if adjustments.marginal_y30 > best_marginal {
                    best_marginal = adjustments.marginal_y30;
                    best_adjustments = adjustments;
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
                        intrinsic_y30: candidate.decision_value.total(),
                    });
                }
                break;
            }
            if best_marginal < self.config.min_marginal_value {
                // min_dispatches: if we haven't selected enough yet and the
                // best remaining candidate has positive value, select it
                // anyway. This breaks the WAIT deadlock by ensuring the
                // brain always tries at least one action.
                if (selected.len() as u32) < self.config.min_dispatches && best_marginal > 0.0 {
                    // Fall through — select this candidate even though
                    // it's below min_marginal_value.
                } else {
                    // All remaining candidates have negative or low marginal
                    // value. Reject the rest and stop.
                    for candidate in remaining.drain(..) {
                        rejected.push(PortfolioRejection {
                            opportunity_key: candidate.opportunity_id.to_string(),
                            reason: if best_marginal < 0.0 {
                                RejectionReason::NegativeMarginalValue
                            } else {
                                RejectionReason::BelowThreshold
                            },
                            intrinsic_y30: candidate.decision_value.total(),
                        });
                    }
                    break;
                }
            }
            // Select the best candidate. swap_remove is O(1) — the
            // remaining order doesn't matter because we rescan each
            // iteration anyway.
            if let Some(idx) = best_idx {
                let candidate = remaining.swap_remove(idx);
                // P0-2: Track experimental slot usage.
                if candidate.is_experimental
                    && selected.len() >= self.config.max_dispatches as usize
                {
                    experimental_slots_used += 1;
                }
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
                // Keyed by the same string the rejections use, so a reader
                // holding an opportunity key can look up either answer.
                marginal_adjustments.insert(candidate.opportunity_id.to_string(), best_adjustments);
                selected.push(candidate);
            } else {
                // No candidate fits the cost budget — reject the rest.
                for candidate in remaining.drain(..) {
                    rejected.push(PortfolioRejection {
                        opportunity_key: candidate.opportunity_id.to_string(),
                        reason: RejectionReason::BudgetExhausted,
                        intrinsic_y30: candidate.decision_value.total(),
                    });
                }
                break;
            }
        }
        // Reject any remaining candidates (all budgets were reached).
        for candidate in remaining {
            rejected.push(PortfolioRejection {
                opportunity_key: candidate.opportunity_id.to_string(),
                reason: RejectionReason::MaxDispatchesReached,
                intrinsic_y30: candidate.decision_value.total(),
            });
        }
        let do_nothing = selected.is_empty();
        PortfolioSelection {
            selected,
            rejected,
            total_expected_fans,
            do_nothing,
            wait_reason: None,
            marginal_adjustments,
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
        //
        // min_dispatches breaks the cold-start deadlock: when the brain has
        // many pending measurements, VOI can exceed the best action's value,
        // making WAIT win every cycle. But measurements only resolve after
        // the brain acts and the measurement window elapses — so WAIT
        // creates a deadlock where the brain never acts. min_dispatches=1
        // guarantees the brain dispatches at least one candidate with
        // positive value, even if WAIT's net utility is positive. WAIT
        // still blocks candidates whose value is below the WAIT threshold.
        let has_positive_candidates = candidates.iter().any(|c| c.decision_value.total() > 0.0);
        // min_dispatches breaks the cold-start deadlock: when min_dispatches > 0
        // and candidates with positive value exist, WAIT cannot block the entire
        // portfolio. This ensures the brain always tries at least one action,
        // even when VOI from pending measurements exceeds the best action's value.
        // When min_dispatches=0, WAIT can still block all candidates.
        let min_dispatches_overrides_wait =
            self.config.min_dispatches > 0 && has_positive_candidates;
        if wait_total > 0.0
            && wait_total > self.config.min_marginal_value
            && !min_dispatches_overrides_wait
        {
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
                    intrinsic_y30: c.decision_value.total(),
                })
                .collect();
            return PortfolioSelection {
                selected: Vec::new(),
                rejected,
                total_expected_fans: 0.0,
                do_nothing: true,
                wait_reason: Some(reason),
                // Nothing was selected, so there is nothing to break down.
                marginal_adjustments: std::collections::BTreeMap::new(),
            };
        }
        // WAIT doesn't win — proceed with normal selection.
        let mut selection = self.select(candidates);
        // If WAIT would have won but min_dispatches forced action, annotate
        // the selection so the operator can see why the brain acted despite
        // WAIT having positive net utility.
        if wait_total > 0.0 && selection.selected.is_empty() && min_dispatches_overrides_wait {
            // This shouldn't happen — select() should return at least
            // min_dispatches candidates when has_positive_candidates is
            // true. But if it does, log the override.
            selection.wait_reason = Some(format!(
                "WAIT overridden by min_dispatches: VOI={:.2}, net_utility={:.2} > 0 \
                 but min_dispatches={} forced action",
                wait.value_of_information, wait_total, self.config.min_dispatches,
            ));
        } else if wait_total > 0.0
            && !selection.selected.is_empty()
            && min_dispatches_overrides_wait
        {
            selection.wait_reason = Some(format!(
                "WAIT overridden by min_dispatches={}: VOI={:.2}, net_utility={:.2} > 0, \
                 dispatched {} candidate(s) with positive value",
                self.config.min_dispatches,
                wait.value_of_information,
                wait_total,
                selection.selected.len(),
            ));
        }
        selection
    }
}
#[cfg(test)]
mod tests;
