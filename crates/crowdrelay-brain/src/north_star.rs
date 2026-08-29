//! North Star — incremental durable fans.
//!
//! The brain's North Star metric is **incremental unique durable fans**:
//! fans that would NOT have arrived without the brain's actions, deduplicated
//! across channels, and still active after 30 days.
//!
//! This is NOT the same as "total fan count after dispatch". The old system
//! measured `(total_fans_at_t+14d - total_fans_at_t) / total_fans_at_t`,
//! which is a correlation metric — it counts ALL new fans, including organic
//! ones that would have arrived anyway.
//!
//! # Counterfactual estimation
//!
//! The brain estimates the counterfactual (what would have happened without
//! the action) using the pre-action daily fan arrival rate:
//!
//! ```text
//! counterfactual = pre_action_daily_rate × measurement_window_days
//! raw_uplift     = observed_new_fans - counterfactual   (signed!)
//! incremental    = max(0, raw_uplift)                   (display only)
//! ```
//!
//! The **signed** `raw_uplift` is what the learning path uses. Clamping to
//! zero destroys negative signal — if an action caused -4 fans, the brain
//! must see -4, not 0, so it can learn that the intervention backfired.
//! The clamped `incremental_fans` is kept for backwards-compatible display
//! and reporting only.
//!
//! # Durability
//!
//! A fan is "durable" if they're still active 30 days after first joining.
//! "Active" means: has a valid Signal endpoint, OR has interacted with a
//! community post, OR has purchased a ticket/merch item. A fan who joins
//! and immediately leaves (or never engages) is not durable.
//!
//! The current `raw_durable_uplift` is computed as `raw_uplift × durability_ratio`,
//! which assumes incremental fans have the same durability as the whole
//! cohort. This is a placeholder — the target is a directly modelled
//! `E[Y_30d(1) - Y_30d(0)]` from experiment data. The field shape is stable
//! so the coefficient can be learned later without breaking the API.

use serde::Serialize;

/// Cap for `uplift_ratio` when the counterfactual is zero but observed fans
/// exist. Keeps the value finite so default JSON serialization does not fail.
const MAX_UPLIFT_RATIO: f64 = 1_000_000.0;

/// The brain's North Star metric: incremental durable fans.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct NorthStarMetric {
    /// New fans observed in the measurement window (14 days post-action).
    pub observed_new_fans: f64,
    /// Estimated fans that would have arrived without the action
    /// (pre-action daily rate × window days).
    pub counterfactual_fans: f64,
    /// **Signed** uplift = observed - counterfactual. Can be negative.
    /// This is what the learning path (causal model, strategy learning)
    /// uses — it preserves the signal that an action backfired.
    pub raw_uplift: f64,
    /// Incremental fans = `max(0, raw_uplift)`. Clamped for display and
    /// backwards-compatible reporting. **Do not use this for learning** —
    /// it destroys negative signal.
    pub incremental_fans: f64,
    /// Fans from the measurement window that are still active after 30 days.
    pub durable_fans: f64,
    /// **Signed** durable uplift = `raw_uplift × durability_ratio`. Can be
    /// negative. Placeholder for a directly modelled durable treatment
    /// effect `E[Y_30d(1) - Y_30d(0)]`.
    pub raw_durable_uplift: f64,
    /// Incremental durable fans = `max(0, raw_durable_uplift)`. The North
    /// Star for display and reporting. **Do not use this for learning**.
    pub incremental_durable_fans: f64,
    /// The pre-action daily fan arrival rate (fans per day).
    pub pre_action_daily_rate: f64,
    /// The measurement window in days (typically 14).
    pub window_days: u32,
}

impl NorthStarMetric {
    /// Computes the North Star metric from raw measurements.
    ///
    /// - `observed_new_fans`: new fans in the 14-day post-action window
    /// - `durable_fans`: fans from that window still active after 30 days
    /// - `pre_action_daily_rate`: average daily fan arrivals in the 30 days
    ///   before the action
    /// - `window_days`: the measurement window (typically 14)
    #[must_use]
    pub fn from_measurements(
        observed_new_fans: f64,
        durable_fans: f64,
        pre_action_daily_rate: f64,
        window_days: u32,
    ) -> Self {
        let counterfactual_fans = pre_action_daily_rate * f64::from(window_days);
        // Signed uplift — the learning signal. Never clamped.
        let raw_uplift = observed_new_fans - counterfactual_fans;
        // Clamped for display/reporting only.
        let incremental_fans = raw_uplift.max(0.0);
        // Durability ratio: what fraction of observed fans are durable?
        let durability_ratio = if observed_new_fans > 0.0 {
            durable_fans / observed_new_fans
        } else {
            0.0
        };
        // Signed durable uplift — placeholder for directly modelled
        // E[Y_30d(1) - Y_30d(0)]. Currently assumes the durability ratio
        // is the same for incremental and organic fans.
        let raw_durable_uplift = raw_uplift * durability_ratio;
        let incremental_durable_fans = raw_durable_uplift.max(0.0);
        Self {
            observed_new_fans,
            counterfactual_fans,
            raw_uplift,
            incremental_fans,
            durable_fans,
            raw_durable_uplift,
            incremental_durable_fans,
            pre_action_daily_rate,
            window_days,
        }
    }

    /// Returns true if this action produced any incremental fans.
    ///
    /// Uses the **signed** `raw_uplift`, not the clamped `incremental_fans`.
    /// An action that produced -4 fans does NOT have an incremental effect.
    #[must_use]
    pub fn has_incremental_effect(self) -> bool {
        self.raw_uplift > 0.0
    }

    /// Returns true if the action **backfired** — produced negative uplift.
    /// The brain uses this to learn that an intervention worsened the
    /// situation, which the old `max(0, ...)` clamp destroyed.
    #[must_use]
    pub fn has_negative_effect(self) -> bool {
        self.raw_uplift < 0.0
    }

    /// Returns the signed uplift rate (raw_uplift per day).
    #[must_use]
    pub fn uplift_daily_rate(self) -> f64 {
        if self.window_days == 0 {
            return 0.0;
        }
        self.raw_uplift / f64::from(self.window_days)
    }

    /// Returns the incremental rate (incremental fans per day).
    ///
    /// Uses the clamped `incremental_fans` for backwards compatibility.
    /// Prefer `uplift_daily_rate` for learning.
    #[must_use]
    pub fn incremental_daily_rate(self) -> f64 {
        if self.window_days == 0 {
            return 0.0;
        }
        self.incremental_fans / f64::from(self.window_days)
    }

    /// Returns the uplift ratio: observed / counterfactual.
    /// Above 1.0 = the action increased fan arrivals above the organic rate.
    /// Below 1.0 = the action may have suppressed organic growth.
    ///
    /// When the counterfactual is zero but observed fans exist, returns a
    /// large finite cap (`MAX_UPLIFT_RATIO`) instead of `f64::INFINITY` so
    /// the value remains JSON-serializable by default.
    #[must_use]
    pub fn uplift_ratio(self) -> f64 {
        if self.counterfactual_fans <= 0.0 {
            return if self.observed_new_fans > 0.0 {
                MAX_UPLIFT_RATIO
            } else {
                1.0
            };
        }
        self.observed_new_fans / self.counterfactual_fans
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_observed_zero_counterfactual_gives_zero_incremental() {
        let metric = NorthStarMetric::from_measurements(0.0, 0.0, 0.0, 14);
        assert_eq!(metric.raw_uplift, 0.0);
        assert_eq!(metric.incremental_fans, 0.0);
        assert_eq!(metric.incremental_durable_fans, 0.0);
        assert!(!metric.has_incremental_effect());
        assert!(!metric.has_negative_effect());
    }

    #[test]
    fn observed_equals_counterfactual_gives_zero_incremental() {
        // Pre-action rate: 1 fan/day → counterfactual = 14 fans in 14 days.
        // Observed: 14 fans → no incremental effect.
        let metric = NorthStarMetric::from_measurements(14.0, 10.0, 1.0, 14);
        assert_eq!(metric.counterfactual_fans, 14.0);
        assert_eq!(metric.raw_uplift, 0.0);
        assert_eq!(metric.incremental_fans, 0.0);
        assert!(!metric.has_incremental_effect());
        assert!(!metric.has_negative_effect());
    }

    #[test]
    fn observed_above_counterfactual_gives_positive_incremental() {
        // Pre-action rate: 1 fan/day → counterfactual = 14.
        // Observed: 20 fans → raw_uplift = 6.
        let metric = NorthStarMetric::from_measurements(20.0, 15.0, 1.0, 14);
        assert_eq!(metric.counterfactual_fans, 14.0);
        assert!((metric.raw_uplift - 6.0).abs() < 0.01);
        assert!((metric.incremental_fans - 6.0).abs() < 0.01);
        assert!(metric.has_incremental_effect());
        assert!(!metric.has_negative_effect());
    }

    #[test]
    fn observed_below_counterfactual_preserves_negative_uplift() {
        // Pre-action rate: 2 fans/day → counterfactual = 28.
        // Observed: 10 fans → raw_uplift = -18 (action backfired!).
        // incremental_fans = max(0, -18) = 0 (display only).
        // raw_uplift = -18 (learning signal preserved).
        let metric = NorthStarMetric::from_measurements(10.0, 5.0, 2.0, 14);
        assert_eq!(metric.counterfactual_fans, 28.0);
        assert!((metric.raw_uplift - (-18.0)).abs() < 0.01);
        assert_eq!(metric.incremental_fans, 0.0);
        assert!(!metric.has_incremental_effect());
        assert!(metric.has_negative_effect());
    }

    #[test]
    fn raw_uplift_feeds_learning_not_incremental_fans() {
        // The key invariant: raw_uplift preserves the sign.
        // incremental_fans destroys it. Learning must use raw_uplift.
        let metric = NorthStarMetric::from_measurements(5.0, 2.0, 2.0, 14);
        assert!(metric.raw_uplift < 0.0);
        assert_eq!(metric.incremental_fans, 0.0);
        assert!(metric.has_negative_effect());
    }

    #[test]
    fn durability_ratio_applies_to_signed_uplift() {
        // 20 observed, 10 durable → 50% durability.
        // raw_uplift = 6 → raw_durable_uplift = 3.
        let metric = NorthStarMetric::from_measurements(20.0, 10.0, 1.0, 14);
        assert!((metric.raw_durable_uplift - 3.0).abs() < 0.01);
        assert!((metric.incremental_durable_fans - 3.0).abs() < 0.01);
    }

    #[test]
    fn negative_uplift_gives_negative_durable_uplift() {
        // 10 observed, 5 durable → 50% durability.
        // raw_uplift = -18 → raw_durable_uplift = -9.
        let metric = NorthStarMetric::from_measurements(10.0, 5.0, 2.0, 14);
        assert!((metric.raw_durable_uplift - (-9.0)).abs() < 0.01);
        // Clamped for display.
        assert_eq!(metric.incremental_durable_fans, 0.0);
    }

    #[test]
    fn zero_observed_gives_zero_durable_incremental() {
        let metric = NorthStarMetric::from_measurements(0.0, 0.0, 1.0, 14);
        assert_eq!(metric.raw_durable_uplift, 0.0);
        assert_eq!(metric.incremental_durable_fans, 0.0);
    }

    #[test]
    fn uplift_ratio_above_one_means_positive_effect() {
        let metric = NorthStarMetric::from_measurements(20.0, 15.0, 1.0, 14);
        assert!((metric.uplift_ratio() - (20.0 / 14.0)).abs() < 0.01);
    }

    #[test]
    fn uplift_ratio_one_means_no_effect() {
        let metric = NorthStarMetric::from_measurements(14.0, 10.0, 1.0, 14);
        assert!((metric.uplift_ratio() - 1.0).abs() < 0.01);
    }

    #[test]
    fn uplift_ratio_capped_when_counterfactual_zero() {
        let metric = NorthStarMetric::from_measurements(5.0, 3.0, 0.0, 14);
        let ratio = metric.uplift_ratio();
        assert!(ratio.is_finite());
        assert_eq!(ratio, MAX_UPLIFT_RATIO);
    }

    #[test]
    fn incremental_daily_rate_normalizes_by_window() {
        let metric = NorthStarMetric::from_measurements(20.0, 15.0, 1.0, 14);
        assert!((metric.incremental_daily_rate() - (6.0 / 14.0)).abs() < 0.01);
    }

    #[test]
    fn uplift_daily_rate_preserves_sign() {
        // Negative uplift → negative daily rate.
        let metric = NorthStarMetric::from_measurements(10.0, 5.0, 2.0, 14);
        assert!(metric.uplift_daily_rate() < 0.0);
        assert!((metric.uplift_daily_rate() - (-18.0 / 14.0)).abs() < 0.01);
    }

    #[test]
    fn zero_window_does_not_divide_by_zero() {
        let metric = NorthStarMetric::from_measurements(10.0, 5.0, 1.0, 0);
        assert_eq!(metric.incremental_daily_rate(), 0.0);
        assert_eq!(metric.uplift_daily_rate(), 0.0);
    }
}
