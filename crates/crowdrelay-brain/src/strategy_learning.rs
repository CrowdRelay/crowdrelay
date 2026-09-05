//! Strategy learning — what the brain has observed about growth strategies.
//!
//! [`StateConditionedStrategyPosterior`] is the only strategy belief in the
//! system. It holds a Normal-Normal posterior over incremental fans per
//! `(strategy, growth_trend, event_proximity)`, updated from resolved growth
//! evidence during the causal model load.
//!
//! # Two duplicates removed
//!
//! This module used to hold three answers to "which growth strategy works":
//! `StrategyLearner` (running averages plus a hand-rolled confidence ramp),
//! `StrategyPosterior` (Normal-Normal, unconditioned, with UCB), and this one.
//! Both of the first two were exported from the crate root and referenced by
//! nothing outside their own tests. Three learners with three different
//! answers, two of which nothing could ever ask, is not redundancy — it is a
//! reader having to determine which of them production runs on. They are gone.
//!
//! # Dormant, not consumed
//!
//! Be precise about what this posterior currently does, because its plumbing
//! suggests more than its wiring delivers. It is written — the infra loader
//! folds resolved evidence into it every cycle, and that write is real. It is
//! also threaded through the whole candidate pipeline by shared reference. But
//! no `predict` or `confidence` call reads it on any decision path: candidate
//! generation binds it and discards it, and `community_engager_candidates`
//! takes it as `_strategy_posterior`.
//!
//! So this is a learner accumulating evidence for a consumer that does not
//! exist yet. That is a defensible place to be — the belief has to have history
//! before it can be trusted with a decision — but it must not be described as
//! influencing one. The strategy that actually orders candidates today is
//! [`crate::strategy::GrowthStrategy`], derived from the world model with
//! hysteresis, and template priority within it comes from measured platform
//! yield. Neither consults this posterior.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ─── State-Conditioned Strategy Posterior (P2.3) ─────────────────────────

/// A strategy posterior conditioned on the world state (growth trend, event
/// proximity). The same strategy might work well in stagnant growth but
/// poorly in accelerating growth. This model learns separate posteriors
/// per (strategy, state) pair.
///
/// The state is discretized into:
/// - `growth_trend`: "stagnant", "decelerating", "steady", "accelerating"
/// - `event_proximity`: "close" (≤7d), "near" (≤30d), "far" (>30d)
///
/// Key: `(strategy, growth_trend, event_proximity)`. Value: `NormalPosterior`
/// over incremental fans. Uses skeptical prior (mean=0, variance=PRIOR_VARIANCE).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StateConditionedStrategyPosterior {
    /// Per-(strategy, state) posteriors. Key: "strategy:growth_trend:event_proximity".
    posteriors: HashMap<String, crate::bayesian::NormalPosterior>,
}

impl StateConditionedStrategyPosterior {
    /// Creates a new state-conditioned strategy posterior.
    #[must_use]
    pub fn new() -> Self {
        Self {
            posteriors: HashMap::new(),
        }
    }

    /// Computes the key for a (strategy, growth_trend, event_proximity) triple.
    fn key(strategy: &str, growth_trend: &str, event_proximity: &str) -> String {
        format!("{strategy}:{growth_trend}:{event_proximity}")
    }

    /// Predicts the expected incremental fans for a strategy in a given state.
    ///
    /// Returns `(mean, variance)`. When no data is available for this
    /// (strategy, state) pair, falls back to the skeptical prior (mean=0).
    #[must_use]
    pub fn predict(&self, strategy: &str, growth_trend: &str, event_proximity: &str) -> (f64, f64) {
        let key = Self::key(strategy, growth_trend, event_proximity);
        match self.posteriors.get(&key) {
            Some(post) => (post.mean, post.variance),
            None => (0.0, crate::PRIOR_VARIANCE),
        }
    }

    /// Updates the posterior for a (strategy, state) pair from an observed
    /// outcome.
    pub fn update(
        &mut self,
        strategy: &str,
        growth_trend: &str,
        event_proximity: &str,
        observed_incremental_fans: f64,
        observation_variance: f64,
    ) {
        let key = Self::key(strategy, growth_trend, event_proximity);
        let prior = crate::bayesian::NormalPosterior::prior(0.0, crate::PRIOR_VARIANCE);
        let entry = self.posteriors.entry(key).or_insert(prior);
        entry.update_signed(observed_incremental_fans, observation_variance);
    }

    /// Returns the confidence (observation count) for a (strategy, state) pair.
    #[must_use]
    pub fn confidence(&self, strategy: &str, growth_trend: &str, event_proximity: &str) -> u32 {
        let key = Self::key(strategy, growth_trend, event_proximity);
        self.posteriors.get(&key).map(|p| p.n).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_conditioned_posterior_starts_empty() {
        let posterior = StateConditionedStrategyPosterior::new();
        let (mean, var) = posterior.predict("content_first", "stagnant", "far");
        // No data → prior mean = 0, variance = PRIOR_VARIANCE
        assert!(
            mean.abs() < 0.01,
            "empty posterior should have mean 0, got {mean}"
        );
        assert!(var > 0.0, "empty posterior should have positive variance");
    }

    #[test]
    fn state_conditioned_posterior_learns_per_state() {
        let mut posterior = StateConditionedStrategyPosterior::new();
        // Strategy "content_first" works well in stagnant growth
        for _ in 0..10 {
            posterior.update("content_first", "stagnant", "far", 10.0, 4.0);
        }
        // But poorly in accelerating growth
        for _ in 0..10 {
            posterior.update("content_first", "accelerating", "far", 2.0, 4.0);
        }
        let (mean_stagnant, _) = posterior.predict("content_first", "stagnant", "far");
        let (mean_accel, _) = posterior.predict("content_first", "accelerating", "far");
        assert!(
            mean_stagnant > mean_accel,
            "stagnant should be higher than accelerating for content_first: {mean_stagnant} vs {mean_accel}"
        );
    }

    #[test]
    fn state_conditioned_posterior_shrinks_low_confidence() {
        let mut posterior = StateConditionedStrategyPosterior::new();
        // One observation at 20
        posterior.update("rare_strategy", "stagnant", "far", 20.0, 4.0);
        let (mean, _) = posterior.predict("rare_strategy", "stagnant", "far");
        // With 1 observation, should shrink toward 0 (skeptical prior)
        assert!(
            mean < 15.0,
            "low confidence should shrink toward 0, got {mean}"
        );
        assert!(
            mean > 0.0,
            "but should be pulled somewhat toward 20, got {mean}"
        );
    }
}
