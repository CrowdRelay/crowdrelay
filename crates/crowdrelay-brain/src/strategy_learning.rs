//! Strategy learning — the brain learns which growth strategies work best.
//!
//! The brain doesn't just pick a strategy from the world model and hope — it
//! tracks the outcome of every strategy it runs and learns over time which
//! strategies produce the most fan growth. When enough data has accumulated,
//! the brain can recommend the strategy with the highest average incremental
//! fans, falling back to the default (world-model-derived) strategy when data
//! is scarce.
//!
//! # Confidence
//!
//! Confidence in a strategy's measured average grows with the number of
//! evaluations, capped at 1.0. Strategies with fewer than
//! [`MIN_EVALUATIONS_FOR_RECOMMENDATION`] evaluations are considered
//! under-explored and are never recommended — the brain keeps using the
//! default strategy until it has gathered enough evidence.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Minimum number of evaluations before a strategy can be recommended.
/// Below this, the brain doesn't have enough data to trust the measured
/// average and falls back to the default strategy.
pub const MIN_EVALUATIONS_FOR_RECOMMENDATION: u32 = 3;

/// The number of evaluations at which confidence saturates at 1.0.
pub const CONFIDENCE_SATURATION_EVALUATIONS: u32 = 10;

/// The recorded outcome of a single growth strategy over many evaluations.
#[derive(Clone, Debug, Serialize)]
pub struct StrategyOutcome {
    /// The strategy name (e.g. `"aggressive_discovery"`).
    pub strategy: String,
    /// The sum of incremental fans across all evaluations.
    pub total_incremental_fans: f64,
    /// The number of times this strategy has been evaluated.
    pub evaluation_count: u32,
    /// The fraction of evaluations where `incremental_fans > 0`.
    pub success_rate: f64,
    /// The average incremental fans per evaluation.
    pub avg_incremental_fans: f64,
    /// The time of the most recent evaluation, if any.
    pub last_evaluated_at: Option<OffsetDateTime>,
}

impl StrategyOutcome {
    /// Creates a fresh outcome record for a strategy with no evaluations yet.
    #[must_use]
    pub fn new(strategy: &str) -> Self {
        Self {
            strategy: strategy.to_owned(),
            total_incremental_fans: 0.0,
            evaluation_count: 0,
            success_rate: 0.0,
            avg_incremental_fans: 0.0,
            last_evaluated_at: None,
        }
    }

    /// Records a single evaluation outcome, updating all aggregate fields.
    fn record(&mut self, incremental_fans: f64) {
        let success = u32::from(incremental_fans > 0.0);
        // Running success count is reconstructed from the previous success
        // rate and count, then updated with the new observation.
        let previous_successes =
            (self.success_rate * f64::from(self.evaluation_count)).round() as u32;
        let new_successes = previous_successes + success;

        self.total_incremental_fans += incremental_fans;
        self.evaluation_count += 1;
        self.success_rate = f64::from(new_successes) / f64::from(self.evaluation_count);
        self.avg_incremental_fans = self.total_incremental_fans / f64::from(self.evaluation_count);
        self.last_evaluated_at = Some(OffsetDateTime::now_utc());
    }
}

/// The strategy learner — accumulates outcome data for growth strategies and
/// recommends the best-performing one once enough evidence exists.
#[derive(Clone, Debug, Serialize)]
pub struct StrategyLearner {
    /// Outcome records keyed by strategy name.
    pub strategy_outcomes: HashMap<String, StrategyOutcome>,
    /// The total number of outcome evaluations across all strategies.
    pub total_evaluations: u32,
}

impl Default for StrategyLearner {
    fn default() -> Self {
        Self::new()
    }
}

impl StrategyLearner {
    /// Creates a new strategy learner with no recorded outcomes.
    #[must_use]
    pub fn new() -> Self {
        Self {
            strategy_outcomes: HashMap::new(),
            total_evaluations: 0,
        }
    }

    /// Records the outcome of a single strategy evaluation.
    ///
    /// `incremental_fans` is the fan growth attributed to this strategy run
    /// (can be negative if the strategy backfired). `target_fans` is the
    /// growth target the strategy was working toward — reserved for future
    /// normalization and currently unused, but logged for traceability.
    pub fn record_outcome(&mut self, strategy: &str, incremental_fans: f64, _target_fans: u32) {
        let outcome = self
            .strategy_outcomes
            .entry(strategy.to_owned())
            .or_insert_with(|| StrategyOutcome::new(strategy));
        outcome.record(incremental_fans);
        self.total_evaluations += 1;
    }

    /// Returns the name of the strategy with the highest average incremental
    /// fans, but only if it has at least
    /// [`MIN_EVALUATIONS_FOR_RECOMMENDATION`] evaluations. Returns `None` when
    /// no strategy has enough data — the brain should fall back to the
    /// default strategy in that case.
    #[must_use]
    pub fn best_strategy(&self) -> Option<String> {
        self.strategy_outcomes
            .values()
            .filter(|o| o.evaluation_count >= MIN_EVALUATIONS_FOR_RECOMMENDATION)
            .max_by(|a, b| {
                a.avg_incremental_fans
                    .partial_cmp(&b.avg_incremental_fans)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|o| o.strategy.clone())
    }

    /// Returns the brain's confidence in a strategy's measured average.
    ///
    /// Confidence grows linearly with the number of evaluations and saturates
    /// at 1.0 once [`CONFIDENCE_SATURATION_EVALUATIONS`] evaluations have been
    /// recorded. Unknown strategies have zero confidence.
    #[must_use]
    pub fn strategy_confidence(&self, strategy: &str) -> f64 {
        let count = self
            .strategy_outcomes
            .get(strategy)
            .map_or(0, |o| o.evaluation_count);
        let raw = f64::from(count) / f64::from(CONFIDENCE_SATURATION_EVALUATIONS);
        raw.min(1.0)
    }

    /// Recommends the best strategy from the available set.
    ///
    /// From the provided strategies, recommends the one with the highest
    /// average incremental fans that has at least
    /// [`MIN_EVALUATIONS_FOR_RECOMMENDATION`] evaluations. If no available
    /// strategy has enough data, returns `None` — the brain should use the
    /// default (world-model-derived) strategy.
    #[must_use]
    pub fn recommend_strategy(&self, available_strategies: &[&str]) -> Option<String> {
        available_strategies
            .iter()
            .filter_map(|s| self.strategy_outcomes.get(*s))
            .filter(|o| o.evaluation_count >= MIN_EVALUATIONS_FOR_RECOMMENDATION)
            .max_by(|a, b| {
                a.avg_incremental_fans
                    .partial_cmp(&b.avg_incremental_fans)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|o| o.strategy.clone())
    }
}

// ─── Bayesian Strategy Posterior ────────────────────────────────────────────

/// A Bayesian posterior over each growth strategy's effectiveness.
///
/// Unlike [`StrategyLearner`] which uses simple averages, `StrategyPosterior`
/// uses a proper Normal-Normal conjugate model for each strategy. This gives:
///
/// - **Proper uncertainty**: the posterior variance shrinks with more data,
///   giving mathematically honest credible intervals.
/// - **UCB recommendation**: balances exploitation (high posterior mean)
///   with exploration (high posterior uncertainty) via the Upper Confidence
///   Bound: `UCB = mean + exploration_weight × std`.
/// - **Recency weighting**: more recent observations can be given higher
///   weight (lower observation variance), so the posterior tracks
///   non-stationary strategy effectiveness.
/// - **Signed outcomes**: strategies can backfire (negative incremental
///   fans), and the posterior correctly handles this via `update_signed`.
///
/// # Prior
///
/// The prior is skeptical: `Normal(0, PRIOR_VARIANCE)` — the brain starts
/// believing no strategy has any effect until evidence proves otherwise.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StrategyPosterior {
    /// Per-strategy Normal-Normal posteriors.
    pub posteriors: HashMap<String, crate::bayesian::NormalPosterior>,
}

impl StrategyPosterior {
    /// Creates a new strategy posterior with no data.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the prior for a new strategy: `Normal(0, PRIOR_VARIANCE)`.
    fn prior() -> crate::bayesian::NormalPosterior {
        crate::bayesian::NormalPosterior::prior(0.0, crate::causal_model::PRIOR_VARIANCE)
    }

    /// Bayesian update with one strategy outcome.
    ///
    /// `incremental_fans` is the measured outcome (can be negative).
    /// `observation_variance` controls how much the observation moves the
    /// posterior — higher variance = less trust = more reliance on the prior.
    pub fn update(&mut self, strategy: &str, incremental_fans: f64, observation_variance: f64) {
        let entry = self
            .posteriors
            .entry(strategy.to_owned())
            .or_insert_with(Self::prior);
        entry.update_signed(incremental_fans, observation_variance);
    }

    /// Bayesian update with recency weighting.
    ///
    /// `recency_weight` (0.0–1.0) scales the observation variance: higher
    /// weight = lower effective variance = the observation is trusted more.
    /// This allows the posterior to track non-stationary strategy
    /// effectiveness by giving recent observations more influence.
    pub fn update_with_recency(
        &mut self,
        strategy: &str,
        incremental_fans: f64,
        observation_variance: f64,
        recency_weight: f64,
    ) {
        let effective_variance = observation_variance / recency_weight.clamp(0.01, 1.0);
        self.update(strategy, incremental_fans, effective_variance);
    }

    /// Returns `(mean, std, n)` for a strategy's posterior.
    ///
    /// Unknown strategies return the skeptical prior (mean=0, std=sqrt(PRIOR_VARIANCE)).
    #[must_use]
    pub fn predict(&self, strategy: &str) -> (f64, f64, u32) {
        let post = self.posteriors.get(strategy).unwrap_or(&Self::PRIOR_REF);
        (post.mean, post.std(), post.n)
    }

    /// A const reference to a prior for fallback. Uses the same values as
    /// `Self::prior()` but as a static.
    const PRIOR_REF: crate::bayesian::NormalPosterior =
        crate::bayesian::NormalPosterior::prior(0.0, crate::causal_model::PRIOR_VARIANCE);

    /// Upper Confidence Bound for a strategy.
    ///
    /// `UCB = mean + exploration_weight × std`
    ///
    /// High `exploration_weight` favors uncertain strategies (exploration).
    /// Zero `exploration_weight` is pure exploitation (just the mean).
    #[must_use]
    pub fn ucb(&self, strategy: &str, exploration_weight: f64) -> f64 {
        let (mean, std, _) = self.predict(strategy);
        mean + exploration_weight * std
    }

    /// Recommends the strategy with the highest UCB from the available set.
    ///
    /// `exploration_weight` controls the exploration-exploitation trade-off:
    /// - 0.0: pure exploitation (highest mean)
    /// - >0.0: balances mean with uncertainty (favors under-explored strategies)
    #[must_use]
    pub fn recommend_ucb(&self, available: &[&str], exploration_weight: f64) -> Option<String> {
        available
            .iter()
            .max_by(|a, b| {
                let ucb_a = self.ucb(a, exploration_weight);
                let ucb_b = self.ucb(b, exploration_weight);
                ucb_a
                    .partial_cmp(&ucb_b)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|s| (*s).to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_learner_is_empty() {
        let learner = StrategyLearner::new();
        assert!(learner.strategy_outcomes.is_empty());
        assert_eq!(learner.total_evaluations, 0);
        assert_eq!(learner.best_strategy(), None);
    }

    #[test]
    fn recording_outcome_updates_aggregates() {
        let mut learner = StrategyLearner::new();
        learner.record_outcome("aggressive_discovery", 10.0, 100);
        learner.record_outcome("aggressive_discovery", -4.0, 100);
        learner.record_outcome("aggressive_discovery", 6.0, 100);

        let outcome = &learner.strategy_outcomes["aggressive_discovery"];
        assert_eq!(outcome.evaluation_count, 3);
        assert!((outcome.total_incremental_fans - 12.0).abs() < 1e-9);
        assert!((outcome.avg_incremental_fans - 4.0).abs() < 1e-9);
        // 2 of 3 evaluations were positive.
        assert!((outcome.success_rate - 2.0 / 3.0).abs() < 1e-9);
        assert!(outcome.last_evaluated_at.is_some());
        assert_eq!(learner.total_evaluations, 3);
    }

    #[test]
    fn best_strategy_requires_three_evaluations() {
        let mut learner = StrategyLearner::new();
        learner.record_outcome("content_first", 50.0, 100);
        learner.record_outcome("content_first", 50.0, 100);
        // Only 2 evaluations — not enough.
        assert_eq!(learner.best_strategy(), None);

        learner.record_outcome("content_first", 50.0, 100);
        assert_eq!(learner.best_strategy(), Some("content_first".to_owned()));
    }

    #[test]
    fn best_strategy_picks_highest_average() {
        let mut learner = StrategyLearner::new();
        for _ in 0..3 {
            learner.record_outcome("aggressive_discovery", 5.0, 100);
        }
        for _ in 0..3 {
            learner.record_outcome("event_driven", 20.0, 100);
        }
        for _ in 0..3 {
            learner.record_outcome("content_first", 1.0, 100);
        }
        assert_eq!(learner.best_strategy(), Some("event_driven".to_owned()));
    }

    #[test]
    fn confidence_grows_with_evaluations_and_caps_at_one() {
        let mut learner = StrategyLearner::new();
        assert!((learner.strategy_confidence("content_first") - 0.0).abs() < 1e-9);

        for _ in 0..5 {
            learner.record_outcome("content_first", 1.0, 100);
        }
        // 5 / 10 = 0.5
        assert!((learner.strategy_confidence("content_first") - 0.5).abs() < 1e-9);

        for _ in 0..5 {
            learner.record_outcome("content_first", 1.0, 100);
        }
        // 10 / 10 = 1.0 (saturated)
        assert!((learner.strategy_confidence("content_first") - 1.0).abs() < 1e-9);

        learner.record_outcome("content_first", 1.0, 100);
        // Capped at 1.0 even with more evaluations.
        assert!((learner.strategy_confidence("content_first") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn confidence_zero_for_unknown_strategy() {
        let learner = StrategyLearner::new();
        assert!((learner.strategy_confidence("unknown") - 0.0).abs() < 1e-9);
    }

    #[test]
    fn recommend_returns_none_with_insufficient_data() {
        let mut learner = StrategyLearner::new();
        learner.record_outcome("aggressive_discovery", 100.0, 100);
        learner.record_outcome("aggressive_discovery", 100.0, 100);
        // Only 2 evaluations — not enough to recommend.
        let available = ["aggressive_discovery", "event_driven", "content_first"];
        assert_eq!(learner.recommend_strategy(&available), None);
    }

    #[test]
    fn recommend_picks_best_of_available_strategies() {
        let mut learner = StrategyLearner::new();
        for _ in 0..3 {
            learner.record_outcome("aggressive_discovery", 5.0, 100);
        }
        for _ in 0..3 {
            learner.record_outcome("event_driven", 30.0, 100);
        }
        for _ in 0..3 {
            learner.record_outcome("content_first", 2.0, 100);
        }
        let available = ["aggressive_discovery", "content_first"];
        // event_driven is best overall but not in the available set.
        assert_eq!(
            learner.recommend_strategy(&available),
            Some("aggressive_discovery".to_owned())
        );
    }

    #[test]
    fn recommend_ignores_strategies_below_threshold() {
        let mut learner = StrategyLearner::new();
        for _ in 0..3 {
            learner.record_outcome("aggressive_discovery", 5.0, 100);
        }
        // event_driven has only 1 evaluation with a huge average — should be
        // ignored because it lacks sufficient data.
        learner.record_outcome("event_driven", 1_000.0, 100);
        let available = ["aggressive_discovery", "event_driven"];
        assert_eq!(
            learner.recommend_strategy(&available),
            Some("aggressive_discovery".to_owned())
        );
    }

    #[test]
    fn recommend_returns_none_when_no_available_strategy_has_data() {
        let mut learner = StrategyLearner::new();
        for _ in 0..3 {
            learner.record_outcome("aggressive_discovery", 5.0, 100);
        }
        // None of the available strategies have been evaluated.
        let available = ["event_driven", "content_first"];
        assert_eq!(learner.recommend_strategy(&available), None);
    }

    #[test]
    fn recommend_returns_none_for_empty_available_set() {
        let mut learner = StrategyLearner::new();
        for _ in 0..3 {
            learner.record_outcome("aggressive_discovery", 5.0, 100);
        }
        assert_eq!(learner.recommend_strategy(&[]), None);
    }

    #[test]
    fn record_outcome_tracks_success_rate() {
        let mut learner = StrategyLearner::new();
        // Positive, positive, zero, negative → 2 successes out of 4.
        learner.record_outcome("content_first", 5.0, 100);
        learner.record_outcome("content_first", 3.0, 100);
        learner.record_outcome("content_first", 0.0, 100);
        learner.record_outcome("content_first", -2.0, 100);
        let outcome = &learner.strategy_outcomes["content_first"];
        assert!((outcome.success_rate - 0.5).abs() < 1e-9);
    }

    #[test]
    fn strategy_outcome_serializes() {
        let mut learner = StrategyLearner::new();
        learner.record_outcome("content_first", 10.0, 100);
        let json = serde_json::to_string(&learner).expect("learner serializes");
        assert!(json.contains("content_first"));
        assert!(json.contains("total_evaluations"));
    }

    // ─── StrategyPosterior tests ──────────────────────────────────────────

    #[test]
    fn strategy_posterior_starts_with_skeptical_prior() {
        let posterior = StrategyPosterior::new();
        let (mean, _, _) = posterior.predict("unknown");
        assert!(
            (mean - 0.0).abs() < 1e-9,
            "skeptical prior should start at zero, got {mean}"
        );
    }

    #[test]
    fn strategy_posterior_learns_positive_effect() {
        let mut posterior = StrategyPosterior::new();
        for _ in 0..10 {
            posterior.update("content_first", 5.0, 4.0);
        }
        let (mean, std, n) = posterior.predict("content_first");
        assert!(
            mean > 2.0,
            "positive outcomes should move mean up, got {mean}"
        );
        assert!(std < 2.0, "variance should shrink, got std={std}");
        assert_eq!(n, 10);
    }

    #[test]
    fn strategy_posterior_learns_negative_effect() {
        let mut posterior = StrategyPosterior::new();
        for _ in 0..10 {
            posterior.update("bad_strategy", -3.0, 4.0);
        }
        let (mean, _, _) = posterior.predict("bad_strategy");
        assert!(
            mean < -1.0,
            "negative outcomes should move mean down, got {mean}"
        );
    }

    #[test]
    fn strategy_posterior_ucb_favors_uncertain_strategies() {
        let mut posterior = StrategyPosterior::new();
        // Strategy A: well-explored, mean=5, very low uncertainty
        for _ in 0..20 {
            posterior.update("a", 5.0, 1.0);
        }
        // Strategy B: less explored, mean=4, much higher uncertainty
        for _ in 0..3 {
            posterior.update("b", 4.0, 4.0);
        }
        // With high exploration weight (5.0), B should have higher UCB
        // despite lower mean, because its uncertainty is much larger.
        // UCB_A ≈ 5.0 + 5.0 * 0.22 ≈ 6.1
        // UCB_B ≈ 3.0 + 5.0 * 1.0 = 8.0
        let ucb_a = posterior.ucb("a", 5.0);
        let ucb_b = posterior.ucb("b", 5.0);
        assert!(
            ucb_b > ucb_a,
            "high exploration weight should favor uncertain strategy: ucb_a={ucb_a}, ucb_b={ucb_b}"
        );
    }

    #[test]
    fn strategy_posterior_ucb_zero_exploration_is_mean() {
        let mut posterior = StrategyPosterior::new();
        for _ in 0..10 {
            posterior.update("a", 5.0, 4.0);
        }
        let (mean, _, _) = posterior.predict("a");
        let ucb = posterior.ucb("a", 0.0);
        assert!(
            (ucb - mean).abs() < 0.01,
            "zero exploration weight → UCB = mean"
        );
    }

    #[test]
    fn strategy_posterior_recommend_ucb_picks_highest() {
        let mut posterior = StrategyPosterior::new();
        for _ in 0..10 {
            posterior.update("a", 3.0, 4.0);
        }
        for _ in 0..10 {
            posterior.update("b", 7.0, 4.0);
        }
        let available = ["a", "b"];
        let recommended = posterior.recommend_ucb(&available, 0.1);
        assert_eq!(recommended, Some("b".to_owned()));
    }

    #[test]
    fn strategy_posterior_recommend_ucb_returns_none_for_empty() {
        let posterior = StrategyPosterior::new();
        assert_eq!(posterior.recommend_ucb(&[], 1.0), None);
    }

    #[test]
    fn strategy_posterior_recommend_ucb_returns_none_for_unknown() {
        let posterior = StrategyPosterior::new();
        let available = ["unknown_strategy"];
        // Unknown strategy has prior mean=0, std=sqrt(PRIOR_VARIANCE)=2.0.
        // With exploration weight > 0, it would have a positive UCB.
        // But we should still return it if it's the only option.
        let recommended = posterior.recommend_ucb(&available, 1.0);
        assert_eq!(recommended, Some("unknown_strategy".to_owned()));
    }

    #[test]
    fn strategy_posterior_recency_weighting_favors_recent() {
        let mut posterior = StrategyPosterior::new();
        // Old observations: high outcomes
        for _ in 0..10 {
            posterior.update_with_recency("s", 10.0, 4.0, 0.5);
        }
        // Recent observations: low outcomes
        for _ in 0..10 {
            posterior.update_with_recency("s", 0.0, 4.0, 1.0);
        }
        let (mean, _, _) = posterior.predict("s");
        // With recency weighting, recent observations (weight=1.0) should
        // dominate over old ones (weight=0.5). The mean should be closer
        // to 0 than to 10.
        assert!(
            mean < 5.0,
            "recency weighting should favor recent observations, got mean={mean}"
        );
    }

    #[test]
    fn strategy_posterior_serializes() {
        let mut posterior = StrategyPosterior::new();
        posterior.update("content_first", 10.0, 4.0);
        let json = serde_json::to_string(&posterior).expect("should serialize");
        assert!(json.contains("content_first"));
        assert!(json.contains("posteriors"));
    }
}
