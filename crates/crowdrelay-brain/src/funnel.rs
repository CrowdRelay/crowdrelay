//! Full funnel model: Reach → Response → Conversion → Durability.
//!
//! The brain's growth funnel has four stages, each with its own conversion
//! rate and uncertainty:
//!
//! 1. **Reach**: How many people were exposed to the content?
//!    (e.g. subreddit subscribers, email recipients, push recipients)
//! 2. **Response**: How many engaged with the content?
//!    (e.g. upvotes, clicks, replies, email opens)
//! 3. **Conversion**: How many became fans?
//!    (e.g. followed, subscribed, joined the Signal)
//! 4. **Durability**: How many are still active after 30 days?
//!    (the Y30 North Star — durable fans, not one-day followers)
//!
//! Each stage has a Beta posterior on its conversion rate, learned from
//! evidence. The funnel predicts:
//!
//! ```text
//! E[durable_fans] = reach × P(response) × P(conversion|response)
//!                 × P(durable|conversion)
//! ```
//!
//! The funnel model is separate from the `ReachConversionModel` (which only
//! models Reach→Conversion) because:
//! - The Response stage captures engagement quality (upvotes, opens).
//! - The Durability stage captures long-term retention (Y30).
//! - Decomposing the funnel lets the brain diagnose WHERE the leak is:
//!   is this template bad at getting responses, bad at converting responders,
//!   or bad at retaining converters?

use serde::{Deserialize, Serialize};

use crate::reach::ReachChannel;

/// Default prior mean for the response rate (engagement rate).
/// Typical Reddit post engagement: ~2% of subscribers upvote.
const DEFAULT_RESPONSE_RATE: f64 = 0.02;

/// Default prior mean for the conversion rate given a response.
/// Of those who engage, ~5% follow.
const DEFAULT_CONVERSION_GIVEN_RESPONSE_RATE: f64 = 0.05;

/// Default prior mean for the durability rate (Y30 retention).
/// Of those who follow, ~60% are still active after 30 days.
const DEFAULT_DURABILITY_RATE: f64 = 0.60;

/// Prior strength (pseudo-count) for all funnel stages.
const FUNNEL_PRIOR_STRENGTH: f64 = 10.0;

/// A Beta posterior for one funnel stage's conversion rate.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BetaStage {
    /// Alpha (successes + prior).
    pub alpha: f64,
    /// Beta (failures + prior).
    pub beta: f64,
}

impl Default for BetaStage {
    fn default() -> Self {
        Self::new(DEFAULT_RESPONSE_RATE)
    }
}

impl BetaStage {
    /// Creates a Beta stage with the given prior mean.
    #[must_use]
    pub fn new(prior_mean: f64) -> Self {
        let alpha = prior_mean * FUNNEL_PRIOR_STRENGTH;
        let beta = (1.0 - prior_mean) * FUNNEL_PRIOR_STRENGTH;
        Self { alpha, beta }
    }

    /// Returns the posterior mean conversion rate.
    #[must_use]
    pub fn mean(&self) -> f64 {
        self.alpha / (self.alpha + self.beta)
    }

    /// Returns the posterior variance.
    #[must_use]
    pub fn variance(&self) -> f64 {
        let s = self.alpha + self.beta;
        self.alpha * self.beta / (s * s * (s + 1.0))
    }

    /// Returns the posterior standard deviation.
    #[must_use]
    pub fn std(&self) -> f64 {
        self.variance().sqrt()
    }

    /// Returns the confidence (total observations beyond the prior).
    #[must_use]
    pub fn confidence(&self) -> u32 {
        ((self.alpha + self.beta - FUNNEL_PRIOR_STRENGTH).max(0.0)) as u32
    }

    /// Updates the posterior with `n` trials and `k` successes.
    pub fn update_count(&mut self, k_success: u32, n_total: u32) {
        debug_assert!(
            k_success <= n_total,
            "k_success ({k_success}) must not exceed n_total ({n_total})"
        );
        self.alpha += f64::from(k_success);
        self.beta += f64::from(n_total.saturating_sub(k_success));
    }

    /// Updates the posterior with a single Bernoulli trial.
    pub fn update(&mut self, success: bool) {
        if success {
            self.alpha += 1.0;
        } else {
            self.beta += 1.0;
        }
    }
}

/// The full funnel model for a single (channel, template) pair.
///
/// Each stage is a `BetaStage` posterior. The funnel predicts durable fans
/// by multiplying the stage rates:
///
/// ```text
/// E[durable_fans] = reach × P(response) × P(conv|resp) × P(durable|conv)
/// ```
///
/// The variance is propagated via the delta method (first-order
/// approximation), giving honest uncertainty that grows with reach.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunnelStage {
    /// P(response | reach) — engagement rate.
    pub response: BetaStage,
    /// P(conversion | response) — follow rate given engagement.
    pub conversion_given_response: BetaStage,
    /// P(durable | conversion) — Y30 retention rate.
    pub durability: BetaStage,
}

impl Default for FunnelStage {
    fn default() -> Self {
        Self::new()
    }
}

impl FunnelStage {
    /// Creates a funnel stage with default priors.
    #[must_use]
    pub fn new() -> Self {
        Self {
            response: BetaStage::new(DEFAULT_RESPONSE_RATE),
            conversion_given_response: BetaStage::new(DEFAULT_CONVERSION_GIVEN_RESPONSE_RATE),
            durability: BetaStage::new(DEFAULT_DURABILITY_RATE),
        }
    }

    /// Returns the overall funnel rate: P(durable | reach).
    #[must_use]
    pub fn overall_rate(&self) -> f64 {
        self.response.mean() * self.conversion_given_response.mean() * self.durability.mean()
    }

    /// Returns the overall funnel rate variance via the delta method.
    /// For independent rates r₁, r₂, r₃ with product R = r₁·r₂·r₃:
    /// Var(R) ≈ R² · (Var(r₁)/r₁² + Var(r₂)/r₂² + Var(r₃)/r₃²)
    #[must_use]
    pub fn overall_variance(&self) -> f64 {
        let r1 = self.response.mean();
        let r2 = self.conversion_given_response.mean();
        let r3 = self.durability.mean();
        let v1 = self.response.variance();
        let v2 = self.conversion_given_response.variance();
        let v3 = self.durability.variance();
        let r = r1 * r2 * r3;
        let cv_sum = v1 / (r1 * r1) + v2 / (r2 * r2) + v3 / (r3 * r3);
        r * r * cv_sum
    }

    /// Returns the overall funnel rate standard deviation.
    #[must_use]
    pub fn overall_std(&self) -> f64 {
        self.overall_variance().sqrt()
    }
}

/// Diagnoses which stage of the funnel has the lowest conversion rate
/// relative to its prior. This tells the brain WHERE the leak is.
#[allow(dead_code)] // TODO: wire into production path (next sprint)
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FunnelLeak {
    /// Response rate is below prior (engagement problem).
    Response,
    /// Conversion rate is below prior (follow-through problem).
    Conversion,
    /// Durability rate is below prior (retention problem).
    Durability,
    /// No significant leak detected.
    None,
}

/// The full funnel model: per-(channel, template) funnel stages.
///
/// The brain learns each stage independently from evidence:
/// - **Response**: from reach events with engagement counts.
/// - **Conversion**: from reach events with conversion counts.
/// - **Durability**: from Y30 outcomes.
///
/// The funnel predicts `E[durable_fans]` and its uncertainty, decomposing
/// the growth process into diagnosable stages.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FunnelModel {
    /// Per-(channel, template) funnel stages.
    /// Key: "{channel}:{template_id}".
    pub stages: std::collections::HashMap<String, FunnelStage>,
}

impl FunnelModel {
    /// Creates a new empty funnel model.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the key for a (channel, template) pair.
    #[must_use]
    fn key(channel: ReachChannel, template_id: &str) -> String {
        format!("{}:{}", channel.as_str(), template_id)
    }

    /// Returns the funnel stage for a (channel, template), or the default.
    #[must_use]
    pub fn stage(&self, channel: ReachChannel, template_id: &str) -> &FunnelStage {
        self.stages
            .get(&Self::key(channel, template_id))
            .unwrap_or(&DEFAULT_STAGE)
    }

    /// Returns a mutable reference to the funnel stage, creating it if needed.
    fn stage_mut(&mut self, channel: ReachChannel, template_id: &str) -> &mut FunnelStage {
        self.stages
            .entry(Self::key(channel, template_id))
            .or_default()
    }

    /// Updates the response stage from engagement counts.
    #[allow(dead_code)] // TODO: wire into production path (next sprint)
    pub fn update_response(
        &mut self,
        channel: ReachChannel,
        template_id: &str,
        n_engaged: u32,
        n_reached: u32,
    ) {
        self.stage_mut(channel, template_id)
            .response
            .update_count(n_engaged, n_reached);
    }

    /// Updates the conversion stage from conversion counts given response.
    #[allow(dead_code)] // TODO: wire into production path (next sprint)
    pub fn update_conversion(
        &mut self,
        channel: ReachChannel,
        template_id: &str,
        n_converted: u32,
        n_engaged: u32,
    ) {
        self.stage_mut(channel, template_id)
            .conversion_given_response
            .update_count(n_converted, n_engaged);
    }

    /// Updates the durability stage from Y30 retention counts.
    #[allow(dead_code)] // TODO: wire into production path (next sprint)
    pub fn update_durability(
        &mut self,
        channel: ReachChannel,
        template_id: &str,
        n_durable: u32,
        n_converted: u32,
    ) {
        self.stage_mut(channel, template_id)
            .durability
            .update_count(n_durable, n_converted);
    }

    /// Predicts durable fans from reach, with uncertainty.
    ///
    /// Returns `(expected_durable_fans, std)`.
    #[allow(dead_code)] // TODO: wire into production path (next sprint)
    #[must_use]
    pub fn predict_durable_fans(
        &self,
        channel: ReachChannel,
        template_id: &str,
        estimated_reach: u32,
    ) -> (f64, f64) {
        let stage = self.stage(channel, template_id);
        let rate = stage.overall_rate();
        let std = stage.overall_std();
        let reach_f = f64::from(estimated_reach);
        (reach_f * rate, reach_f * std)
    }

    /// Diagnoses the funnel for a (channel, template).
    #[allow(dead_code)] // TODO: wire into production path (next sprint)
    #[must_use]
    pub fn diagnose(&self, channel: ReachChannel, template_id: &str) -> FunnelLeak {
        let stage = self.stage(channel, template_id);
        let resp_ratio = stage.response.mean() / DEFAULT_RESPONSE_RATE;
        let conv_ratio =
            stage.conversion_given_response.mean() / DEFAULT_CONVERSION_GIVEN_RESPONSE_RATE;
        let dur_ratio = stage.durability.mean() / DEFAULT_DURABILITY_RATE;
        let min_ratio = resp_ratio.min(conv_ratio).min(dur_ratio);
        if min_ratio == resp_ratio && resp_ratio < 0.8 {
            FunnelLeak::Response
        } else if min_ratio == conv_ratio && conv_ratio < 0.8 {
            FunnelLeak::Conversion
        } else if min_ratio == dur_ratio && dur_ratio < 0.8 {
            FunnelLeak::Durability
        } else {
            FunnelLeak::None
        }
    }
}

/// A default funnel stage used as a fallback when no data exists.
static DEFAULT_STAGE: std::sync::LazyLock<FunnelStage> = std::sync::LazyLock::new(FunnelStage::new);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beta_stage_prior_mean() {
        let stage = BetaStage::new(0.1);
        assert!((stage.mean() - 0.1).abs() < 0.01);
    }

    #[test]
    fn beta_stage_updates_correctly() {
        let mut stage = BetaStage::new(0.5);
        stage.update_count(8, 10);
        // Prior: alpha=5, beta=5. After: alpha=13, beta=7.
        // Mean = 13/20 = 0.65
        assert!((stage.mean() - 0.65).abs() < 0.01);
        assert_eq!(stage.confidence(), 10);
    }

    #[test]
    fn funnel_overall_rate_is_product() {
        let stage = FunnelStage::new();
        let expected = 0.02 * 0.05 * 0.60;
        assert!((stage.overall_rate() - expected).abs() < 1e-6);
    }

    #[test]
    fn funnel_predict_durable_fans() {
        let model = FunnelModel::new();
        let (fans, std) = model.predict_durable_fans(ReachChannel::RedditPost, "t", 1000);
        // Default rate = 0.02 * 0.05 * 0.60 = 0.0006
        // E[durable] = 1000 * 0.0006 = 0.6
        assert!((fans - 0.6).abs() < 0.01);
        assert!(std > 0.0);
    }

    #[test]
    fn funnel_diagnoses_response_leak() {
        let mut model = FunnelModel::new();
        // Low engagement: 1 out of 1000 reached engaged.
        model.update_response(ReachChannel::RedditPost, "t", 1, 1000);
        let leak = model.diagnose(ReachChannel::RedditPost, "t");
        assert_eq!(leak, FunnelLeak::Response);
    }

    #[test]
    fn funnel_diagnoses_durability_leak() {
        let mut model = FunnelModel::new();
        // Good response and conversion, bad durability.
        model.update_response(ReachChannel::RedditPost, "t", 100, 1000);
        model.update_conversion(ReachChannel::RedditPost, "t", 50, 100);
        // Only 1 out of 50 converters is durable.
        model.update_durability(ReachChannel::RedditPost, "t", 1, 50);
        let leak = model.diagnose(ReachChannel::RedditPost, "t");
        assert_eq!(leak, FunnelLeak::Durability);
    }

    #[test]
    fn funnel_no_leak_when_all_at_prior() {
        let model = FunnelModel::new();
        let leak = model.diagnose(ReachChannel::RedditPost, "t");
        assert_eq!(leak, FunnelLeak::None);
    }

    #[test]
    fn funnel_uncertainty_grows_with_reach() {
        let model = FunnelModel::new();
        let (_, std_small) = model.predict_durable_fans(ReachChannel::RedditPost, "t", 100);
        let (_, std_large) = model.predict_durable_fans(ReachChannel::RedditPost, "t", 10000);
        assert!(
            std_large > std_small,
            "uncertainty should grow with reach, got small={std_small:.6} large={std_large:.6}"
        );
    }

    #[test]
    fn funnel_uncertainty_shrinks_with_observations() {
        let mut model = FunnelModel::new();
        let (_, std_before) = model.predict_durable_fans(ReachChannel::RedditPost, "t", 1000);
        // Add many observations to all stages.
        for _ in 0..100 {
            model.update_response(ReachChannel::RedditPost, "t", 20, 1000);
            model.update_conversion(ReachChannel::RedditPost, "t", 5, 20);
            model.update_durability(ReachChannel::RedditPost, "t", 3, 5);
        }
        let (_, std_after) = model.predict_durable_fans(ReachChannel::RedditPost, "t", 1000);
        assert!(
            std_after < std_before,
            "uncertainty should shrink with observations, got before={std_before:.6} after={std_after:.6}"
        );
    }
}
