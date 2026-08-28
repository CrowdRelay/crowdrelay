//! Offline world-model simulation — the brain's "imagination" layer.
//!
//! Before dispatching, the brain can simulate what would happen under
//! different strategies. It runs mental simulations using the current
//! [`WorldModel`] (state) and the learned [`CausalModel`] (effects) to
//! evaluate which strategy would produce the most incremental fans.
//!
//! This is not a crystal ball — the predictions carry the causal model's
//! uncertainty as a confidence score, so the brain knows when it's guessing
//! versus when it has solid evidence.
//!
//! # How it works
//!
//! For each month of the simulation:
//! 1. The strategy's template priority order determines which templates get
//!    dispatched.
//! 2. The first template gets its full `expected_fans` from the causal model;
//!    subsequent templates get diminishing returns (`0.8^rank`).
//! 3. Fans accumulate month over month (with organic growth compounding on
//!    top of the strategy-driven acquisition).
//! 4. Signal installs are predicted from the causal model's Signal posterior.
//!
//! The organic growth baseline (what would happen with no strategy) is
//! computed from the world model's `fan_growth_rate_bps` and subtracted to
//! produce `predicted_incremental_fans`.

use serde::Serialize;

use crate::causal_model::{CausalModel, DispatchContext};
use crate::strategy::GrowthStrategy;
use crate::world_model::WorldModel;

/// Diminishing-returns factor applied to each template after the first in a
/// strategy's priority list. The rank-0 template gets full expected fans;
/// rank-1 gets `0.8×`, rank-2 gets `0.64×`, and so on. This models the brain's
/// belief that concentrating effort on the top-priority template yields
/// diminishing marginal returns as effort spreads across more templates.
const DIMINISHING_RETURNS_FACTOR: f64 = 0.8;

/// The result of simulating a single strategy over a number of months.
///
/// The brain runs many of these (one per candidate strategy) and compares
/// them to pick the strategy with the highest predicted incremental fans,
/// weighted by confidence.
#[derive(Clone, Debug, Serialize)]
pub struct SimulationResult {
    /// The strategy that was simulated.
    pub strategy: GrowthStrategy,
    /// Total fans predicted after the simulation period (organic + strategy).
    pub predicted_fans: u32,
    /// Fans predicted above the organic growth baseline — the brain's North
    /// Star metric for strategy evaluation. This is what the strategy
    /// *adds* on top of doing nothing.
    pub predicted_incremental_fans: f64,
    /// Total Signal installs predicted after the simulation period.
    pub predicted_signal_installs: u32,
    /// How confident the brain is in this prediction, in `[0.0, 1.0]`.
    /// Derived from the causal model's posterior uncertainty: high confidence
    /// means the brain has enough data to trust the prediction; low
    /// confidence means it should explore more before committing.
    pub confidence: f64,
    /// Month-by-month breakdown of the simulation, for inspection and
    /// debugging. The brain can show this to the operator to explain *why*
    /// it chose a strategy.
    pub monthly_breakdown: Vec<MonthlyPrediction>,
}

/// One month's worth of predicted growth within a simulation.
#[derive(Clone, Debug, Serialize)]
pub struct MonthlyPrediction {
    /// The month index (1-based).
    pub month: u32,
    /// New fans predicted this month (strategy-driven + organic).
    pub new_fans: f64,
    /// Cumulative fans at the end of this month.
    pub cumulative_fans: u32,
    /// New Signal installs predicted this month.
    pub new_signal_installs: f64,
}

/// The brain's offline world-model simulator — the "imagination" layer.
///
/// Holds a snapshot of the current [`WorldModel`] (the brain's belief about
/// the world) and the learned [`CausalModel`] (the brain's belief about
/// effects). The simulator is read-only: it never mutates either model. It
/// runs purely mental simulations to evaluate strategies before dispatching.
///
/// # Example
///
/// ```
/// use crowdrelay_brain::{
///     simulation::WorldSimulation, CausalModel, WorldModel, GrowthStrategy,
/// };
///
/// let world = WorldModel::default();
/// let causal = CausalModel::new();
/// let sim = WorldSimulation::new(world, causal);
///
/// let strategies = [
///     GrowthStrategy::AggressiveDiscovery,
///     GrowthStrategy::ContentFirst,
/// ];
/// let results = sim.compare_strategies(&strategies, 3);
/// // Results are sorted by predicted incremental fans, best first.
/// assert!(!results.is_empty());
/// ```
pub struct WorldSimulation {
    /// The current world state — the brain's belief about fans, Signal
    /// installs, communities, and growth trend.
    world_model: WorldModel,
    /// The learned causal model — the brain's belief about how many fans
    /// each template produces, with uncertainty.
    causal_model: CausalModel,
}

impl WorldSimulation {
    /// Creates a new simulator from the current world state and causal model.
    ///
    /// The simulator borrows the models by value (clone) so that the
    /// simulation never mutates the brain's live state.
    #[must_use]
    pub fn new(world_model: WorldModel, causal_model: CausalModel) -> Self {
        Self {
            world_model,
            causal_model,
        }
    }

    /// Simulates a single strategy over `months` months and returns the
    /// predicted outcome.
    ///
    /// For each month:
    /// - The strategy's template priority list determines which templates are
    ///   "dispatched" in the simulation.
    /// - The rank-0 template gets its full `expected_fans` from the causal
    ///   model; rank-*n* gets `expected_fans × 0.8^n` (diminishing returns).
    /// - Organic growth (from `fan_growth_rate_bps`) compounds on top.
    /// - Signal installs are predicted from the causal model's Signal
    ///   posterior for each template (with the same diminishing-returns
    ///   weighting).
    ///
    /// Confidence is computed as `1.0 - avg(posterior_std / expected_fans)`
    /// across the strategy's templates, clamped to `[0.0, 1.0]`. When the
    /// causal model has low uncertainty (many observations, tight posterior),
    /// confidence is high; when it's guessing (prior only, wide posterior),
    /// confidence is low.
    #[must_use]
    pub fn simulate_strategy(&self, strategy: GrowthStrategy, months: u32) -> SimulationResult {
        let templates = strategy.template_priority();
        let context = self.context_from_world();

        // ── Confidence: how much does the brain trust its predictions? ──
        let confidence = self.compute_confidence(templates);

        // ── Organic growth baseline (what happens with no strategy) ──
        let organic_monthly_rate = f64::from(self.world_model.fan_growth_rate_bps) / 10_000.0;
        let organic_baseline_fans = self.compute_organic_baseline(
            self.world_model.total_fans,
            organic_monthly_rate,
            months,
        );

        // ── Month-by-month simulation ──
        let mut cumulative_fans = f64::from(self.world_model.total_fans);
        let mut cumulative_signal = f64::from(self.world_model.total_signal_installs);
        let mut monthly_breakdown = Vec::with_capacity(usize::try_from(months).unwrap_or(0));

        for month in 1..=months {
            // Strategy-driven fan acquisition with diminishing returns.
            let strategy_fans = self.predict_monthly_strategy_fans(templates, &context);
            // Organic growth compounds on the current fanbase.
            let organic_fans = cumulative_fans * organic_monthly_rate;
            let new_fans = strategy_fans + organic_fans;
            cumulative_fans += new_fans;

            // Signal installs from the strategy (with diminishing returns).
            let new_signal = self.predict_monthly_strategy_signal(templates, &context);
            cumulative_signal += new_signal;

            monthly_breakdown.push(MonthlyPrediction {
                month,
                new_fans,
                cumulative_fans: cumulative_fans.round() as u32,
                new_signal_installs: new_signal,
            });
        }

        let predicted_fans = cumulative_fans.round() as u32;
        let predicted_signal_installs = cumulative_signal.round() as u32;
        let predicted_incremental_fans = cumulative_fans - organic_baseline_fans;

        SimulationResult {
            strategy,
            predicted_fans,
            predicted_incremental_fans,
            predicted_signal_installs,
            confidence,
            monthly_breakdown,
        }
    }

    /// Simulates multiple strategies and returns the results sorted by
    /// predicted incremental fans, descending. The best strategy is first.
    ///
    /// This is the brain's strategy-selection primitive: it simulates every
    /// candidate strategy, then picks the one with the highest predicted
    /// incremental fans (optionally weighted by confidence).
    #[must_use]
    pub fn compare_strategies(
        &self,
        strategies: &[GrowthStrategy],
        months: u32,
    ) -> Vec<SimulationResult> {
        let mut results: Vec<SimulationResult> = strategies
            .iter()
            .map(|&s| self.simulate_strategy(s, months))
            .collect();
        // Sort by predicted incremental fans, descending. Ties keep their
        // original relative order (Rust's sort is stable).
        results.sort_by(|a, b| {
            b.predicted_incremental_fans
                .partial_cmp(&a.predicted_incremental_fans)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results
    }

    // ────────────────────────────────────────────────────────────────────
    // Internal helpers
    // ────────────────────────────────────────────────────────────────────

    /// Builds a `DispatchContext` from the current world model, so that the
    /// causal model's context adjustments (event proximity, growth trend)
    /// are applied consistently in the simulation.
    fn context_from_world(&self) -> DispatchContext {
        DispatchContext {
            days_to_event: self.world_model.days_to_next_event,
            fan_growth_trend: self.world_model.fan_growth_trend,
            subreddit_type: None,
            post_format: None,
            time_of_day_bps: 0,
            community_novelty_bps: 0,
        }
    }

    /// Predicts the total strategy-driven fans for one month, applying
    /// diminishing returns across the template priority list.
    ///
    /// Rank 0 gets full `expected_fans`; rank *n* gets
    /// `expected_fans × 0.8^n`.
    fn predict_monthly_strategy_fans(&self, templates: &[&str], context: &DispatchContext) -> f64 {
        let mut total = 0.0_f64;
        for (rank, template) in templates.iter().enumerate() {
            let base = self.causal_model.expected_fans(template);
            // Apply context adjustments (event proximity, growth trend) on
            // top of the raw posterior mean, same as `CausalModel::predict`.
            let adjusted = self.apply_context_adjustments(base, context);
            let weight = DIMINISHING_RETURNS_FACTOR.powi(i32::try_from(rank).unwrap_or(0));
            total += adjusted * weight;
        }
        total.max(0.0)
    }

    /// Predicts the total strategy-driven Signal installs for one month,
    /// with the same diminishing-returns weighting as fans.
    fn predict_monthly_strategy_signal(
        &self,
        templates: &[&str],
        context: &DispatchContext,
    ) -> f64 {
        let mut total = 0.0_f64;
        for (rank, template) in templates.iter().enumerate() {
            let base = self.causal_model.predict_signal(template, context);
            let weight = DIMINISHING_RETURNS_FACTOR.powi(i32::try_from(rank).unwrap_or(0));
            total += base * weight;
        }
        total.max(0.0)
    }

    /// Applies the causal model's context adjustments (event proximity,
    /// growth trend) to a raw posterior mean. This mirrors the logic in
    /// [`CausalModel::predict`] so the simulation uses the same multiplicative
    /// adjustments without calling `predict` (which would re-query the
    /// hierarchical posterior).
    fn apply_context_adjustments(&self, mut prediction: f64, context: &DispatchContext) -> f64 {
        if let Some(days) = context.days_to_event {
            if days <= 7 {
                prediction *= 1.5;
            } else if days <= 30 {
                prediction *= 1.2;
            }
        }
        match context.fan_growth_trend {
            crate::world_model::GrowthTrend::Stagnant
            | crate::world_model::GrowthTrend::Decelerating => prediction *= 0.8,
            crate::world_model::GrowthTrend::Accelerating => prediction *= 1.1,
            crate::world_model::GrowthTrend::Steady => {}
        }
        prediction.max(0.0)
    }

    /// Computes the brain's confidence in its predictions for the given
    /// templates.
    ///
    /// Confidence = `1.0 - avg(posterior_std / expected_fans)`, clamped to
    /// `[0.0, 1.0]`. When `expected_fans` is zero or near-zero, the ratio is
    /// treated as 1.0 (no confidence) to avoid division-by-zero.
    fn compute_confidence(&self, templates: &[&str]) -> f64 {
        if templates.is_empty() {
            return 0.0;
        }
        let mut sum_ratios = 0.0_f64;
        for template in templates {
            let std = self.causal_model.predict_std(template);
            let expected = self.causal_model.expected_fans(template);
            let ratio = if expected.abs() < f64::EPSILON {
                // No expected fans → the brain has no signal to be confident about.
                1.0
            } else {
                (std / expected).min(1.0)
            };
            sum_ratios += ratio;
        }
        let avg_ratio = sum_ratios / f64::from(u32::try_from(templates.len()).unwrap_or(1));
        (1.0 - avg_ratio).clamp(0.0, 1.0)
    }

    /// Computes the organic growth baseline — the fan count after `months`
    /// months of compounding organic growth, with no strategy intervention.
    ///
    /// This is `total_fans × (1 + monthly_rate)^months`.
    fn compute_organic_baseline(&self, starting_fans: u32, monthly_rate: f64, months: u32) -> f64 {
        let mut fans = f64::from(starting_fans);
        for _ in 0..months {
            fans *= 1.0 + monthly_rate;
        }
        fans
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::causal_model::{DispatchPrediction, PredictionOutcome};
    use crate::world_model::{GrowthTrend, WorldModel};

    // ────────────────────────────────────────────────────────────────────
    // Simulation with default model
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn simulation_with_default_model_produces_positive_fans() {
        let world = WorldModel::default();
        let causal = CausalModel::new();
        let sim = WorldSimulation::new(world, causal);

        let result = sim.simulate_strategy(GrowthStrategy::ContentFirst, 3);

        // Default world model has 0 fans, so predicted_fans comes purely
        // from strategy-driven acquisition (organic rate is 0).
        assert!(
            result.predicted_fans > 0,
            "simulation should predict some fans from strategy, got {}",
            result.predicted_fans
        );
        // With 0 organic growth, incremental == total.
        assert!(
            (result.predicted_incremental_fans - f64::from(result.predicted_fans)).abs() < 0.5,
            "with zero organic growth, incremental should equal total"
        );
        // 3 months → 3 monthly predictions.
        assert_eq!(result.monthly_breakdown.len(), 3);
    }

    #[test]
    fn simulation_accumulates_fans_month_over_month() {
        let world = WorldModel {
            total_fans: 100,
            fan_growth_rate_bps: 500, // 5% monthly
            ..Default::default()
        };
        let causal = CausalModel::new();
        let sim = WorldSimulation::new(world, causal);

        let result = sim.simulate_strategy(GrowthStrategy::AggressiveDiscovery, 4);

        // Cumulative fans should be strictly increasing month over month.
        for window in result.monthly_breakdown.windows(2) {
            assert!(
                window[1].cumulative_fans >= window[0].cumulative_fans,
                "cumulative fans should never decrease: month {} = {}, month {} = {}",
                window[0].month,
                window[0].cumulative_fans,
                window[1].month,
                window[1].cumulative_fans
            );
        }
        // Final cumulative should match predicted_fans.
        if let Some(last) = result.monthly_breakdown.last() {
            assert_eq!(last.cumulative_fans, result.predicted_fans);
        }
    }

    // ────────────────────────────────────────────────────────────────────
    // Strategy comparison
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn compare_strategies_returns_sorted_by_incremental_fans() {
        let world = WorldModel {
            total_fans: 50,
            ..Default::default()
        };
        let causal = CausalModel::new();
        let sim = WorldSimulation::new(world, causal);

        let strategies = [
            GrowthStrategy::AggressiveDiscovery,
            GrowthStrategy::EventDriven,
            GrowthStrategy::ContentFirst,
            GrowthStrategy::SignalConversion,
        ];
        let results = sim.compare_strategies(&strategies, 3);

        assert_eq!(results.len(), strategies.len());
        // Verify descending order.
        for window in results.windows(2) {
            assert!(
                window[0].predicted_incremental_fans >= window[1].predicted_incremental_fans,
                "results should be sorted by incremental fans descending: {} vs {}",
                window[0].predicted_incremental_fans,
                window[1].predicted_incremental_fans
            );
        }
    }

    #[test]
    fn compare_strategies_with_empty_input_returns_empty() {
        let world = WorldModel::default();
        let causal = CausalModel::new();
        let sim = WorldSimulation::new(world, causal);

        let results = sim.compare_strategies(&[], 3);
        assert!(results.is_empty());
    }

    #[test]
    fn compare_strategies_with_learned_model_prefers_effective_template_strategy() {
        // Train the causal model so that "social-post" (ContentFirst's top
        // template) produces many fans, while "press-pitch" (EventDriven's
        // top) produces few.
        let mut causal = CausalModel::new();
        for _ in 0..20 {
            let p = DispatchPrediction {
                template_id: "social-post".to_owned(),
                expected_new_fans: 2.0,
                ..Default::default()
            };
            causal.update(&PredictionOutcome::from_observation(p, 10.0, 1.0));
        }
        for _ in 0..20 {
            let p = DispatchPrediction {
                template_id: "press-pitch".to_owned(),
                expected_new_fans: 2.0,
                ..Default::default()
            };
            causal.update(&PredictionOutcome::from_observation(p, 0.0, 0.0));
        }

        let world = WorldModel::default();
        let sim = WorldSimulation::new(world, causal);

        let strategies = [GrowthStrategy::ContentFirst, GrowthStrategy::EventDriven];
        let results = sim.compare_strategies(&strategies, 3);

        // ContentFirst (social-post first) should beat EventDriven (press-pitch first).
        assert_eq!(
            results[0].strategy,
            GrowthStrategy::ContentFirst,
            "strategy with the effective template should rank first"
        );
    }

    // ────────────────────────────────────────────────────────────────────
    // Confidence computation
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn confidence_is_low_for_untrained_model() {
        // With the default prior (no observations), the posterior std is
        // sqrt(PRIOR_VARIANCE) = 2.0 and expected_fans = 2.0, so
        // std/expected = 1.0 → confidence = 0.0.
        let world = WorldModel::default();
        let causal = CausalModel::new();
        let sim = WorldSimulation::new(world, causal);

        let result = sim.simulate_strategy(GrowthStrategy::ContentFirst, 1);
        assert!(
            result.confidence < 0.01,
            "untrained model should have near-zero confidence, got {}",
            result.confidence
        );
    }

    #[test]
    fn confidence_increases_with_observations() {
        // Train the causal model on all templates in ContentFirst's priority
        // list with consistent observations → posterior shrinks → confidence
        // rises.
        let mut causal = CausalModel::new();
        for template in GrowthStrategy::ContentFirst.template_priority() {
            for _ in 0..20 {
                let p = DispatchPrediction {
                    template_id: (*template).to_owned(),
                    expected_new_fans: 2.0,
                    ..Default::default()
                };
                causal.update(&PredictionOutcome::from_observation(p, 5.0, 0.5));
            }
        }

        let world = WorldModel::default();
        let sim = WorldSimulation::new(world, causal);

        let result = sim.simulate_strategy(GrowthStrategy::ContentFirst, 1);
        assert!(
            result.confidence > 0.5,
            "trained model should have high confidence, got {}",
            result.confidence
        );
    }

    #[test]
    fn confidence_is_clamped_to_unit_interval() {
        let world = WorldModel::default();
        let causal = CausalModel::new();
        let sim = WorldSimulation::new(world, causal);

        let result = sim.simulate_strategy(GrowthStrategy::AggressiveDiscovery, 1);
        assert!(
            (0.0..=1.0).contains(&result.confidence),
            "confidence should be in [0, 1], got {}",
            result.confidence
        );
    }

    // ────────────────────────────────────────────────────────────────────
    // Monthly breakdown accumulation
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn monthly_breakdown_has_correct_month_indices() {
        let world = WorldModel::default();
        let causal = CausalModel::new();
        let sim = WorldSimulation::new(world, causal);

        let result = sim.simulate_strategy(GrowthStrategy::ContentFirst, 5);
        let months: Vec<u32> = result.monthly_breakdown.iter().map(|m| m.month).collect();
        assert_eq!(months, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn monthly_breakdown_new_fans_sum_approximates_total() {
        let world = WorldModel {
            total_fans: 0,
            fan_growth_rate_bps: 0, // no organic growth → sum of new_fans == total
            ..Default::default()
        };
        let causal = CausalModel::new();
        let sim = WorldSimulation::new(world, causal);

        let result = sim.simulate_strategy(GrowthStrategy::ContentFirst, 4);
        let sum_new_fans: f64 = result.monthly_breakdown.iter().map(|m| m.new_fans).sum();
        assert!(
            (sum_new_fans - f64::from(result.predicted_fans)).abs() < 0.5,
            "sum of monthly new_fans ({sum_new_fans}) should approximate predicted_fans ({})",
            result.predicted_fans
        );
    }

    #[test]
    fn monthly_breakdown_cumulative_matches_predicted_fans() {
        let world = WorldModel {
            total_fans: 200,
            fan_growth_rate_bps: 300,
            ..Default::default()
        };
        let causal = CausalModel::new();
        let sim = WorldSimulation::new(world, causal);

        let result = sim.simulate_strategy(GrowthStrategy::EventDriven, 6);
        if let Some(last) = result.monthly_breakdown.last() {
            assert_eq!(
                last.cumulative_fans, result.predicted_fans,
                "last month's cumulative_fans should equal predicted_fans"
            );
        }
    }

    #[test]
    fn monthly_breakdown_signal_installs_accumulate() {
        let world = WorldModel::default();
        let causal = CausalModel::new();
        let sim = WorldSimulation::new(world, causal);

        let result = sim.simulate_strategy(GrowthStrategy::SignalConversion, 3);
        // Each month should predict some signal installs (default is 10% of
        // fan prediction, which is > 0).
        for month in &result.monthly_breakdown {
            assert!(
                month.new_signal_installs > 0.0,
                "each month should predict some signal installs, month {} got {}",
                month.month,
                month.new_signal_installs
            );
        }
        // Predicted total should be >= starting installs (0 by default).
        assert!(result.predicted_signal_installs > 0);
    }

    // ────────────────────────────────────────────────────────────────────
    // Context adjustments
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn simulation_applies_event_proximity_boost() {
        let world_no_event = WorldModel::default();
        let world_with_event = WorldModel {
            days_to_next_event: Some(5),
            ..Default::default()
        };
        let causal = CausalModel::new();

        let sim_no_event = WorldSimulation::new(world_no_event, causal.clone());
        let sim_with_event = WorldSimulation::new(world_with_event, causal);

        let result_no_event = sim_no_event.simulate_strategy(GrowthStrategy::ContentFirst, 1);
        let result_with_event = sim_with_event.simulate_strategy(GrowthStrategy::ContentFirst, 1);

        assert!(
            result_with_event.predicted_fans > result_no_event.predicted_fans,
            "event proximity should boost predicted fans: with_event={}, no_event={}",
            result_with_event.predicted_fans,
            result_no_event.predicted_fans
        );
    }

    #[test]
    fn simulation_applies_stagnant_trend_penalty() {
        let world_steady = WorldModel::default();
        let world_stagnant = WorldModel {
            fan_growth_trend: GrowthTrend::Stagnant,
            ..Default::default()
        };
        let causal = CausalModel::new();

        let sim_steady = WorldSimulation::new(world_steady, causal.clone());
        let sim_stagnant = WorldSimulation::new(world_stagnant, causal);

        let result_steady = sim_steady.simulate_strategy(GrowthStrategy::ContentFirst, 1);
        let result_stagnant = sim_stagnant.simulate_strategy(GrowthStrategy::ContentFirst, 1);

        assert!(
            result_stagnant.predicted_fans < result_steady.predicted_fans,
            "stagnant trend should reduce predicted fans: stagnant={}, steady={}",
            result_stagnant.predicted_fans,
            result_steady.predicted_fans
        );
    }

    // ────────────────────────────────────────────────────────────────────
    // Serialization
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn simulation_result_serializes_to_json() {
        let world = WorldModel::default();
        let causal = CausalModel::new();
        let sim = WorldSimulation::new(world, causal);

        let result = sim.simulate_strategy(GrowthStrategy::ContentFirst, 2);
        let json = serde_json::to_string(&result);
        assert!(json.is_ok(), "SimulationResult should serialize to JSON");

        let monthly_json = serde_json::to_string(&result.monthly_breakdown);
        assert!(monthly_json.is_ok(), "MonthlyPrediction should serialize");
    }
}
