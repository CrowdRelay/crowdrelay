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
//! incremental = max(0, observed_new_fans - counterfactual)
//! ```
//!
//! This is a synthetic control approach — simple but mathematically honest.
//! It's not as rigorous as a randomized experiment (Phase 5 will add that),
//! but it's far better than counting total fans.
//!
//! # Durability
//!
//! A fan is "durable" if they're still active 30 days after first joining.
//! "Active" means: has a valid Signal endpoint, OR has interacted with a
//! community post, OR has purchased a ticket/merch item. A fan who joins
//! and immediately leaves (or never engages) is not durable.

use serde::Serialize;

/// The brain's North Star metric: incremental durable fans.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct NorthStarMetric {
    /// New fans observed in the measurement window (14 days post-action).
    pub observed_new_fans: f64,
    /// Estimated fans that would have arrived without the action
    /// (pre-action daily rate × window days).
    pub counterfactual_fans: f64,
    /// Incremental fans = observed - counterfactual (clamped to ≥0).
    pub incremental_fans: f64,
    /// Fans from the measurement window that are still active after 30 days.
    pub durable_fans: f64,
    /// Incremental durable fans = the North Star. This is what the brain
    /// optimizes for.
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
        let incremental_fans = (observed_new_fans - counterfactual_fans).max(0.0);
        // Durability ratio: what fraction of observed fans are durable?
        let durability_ratio = if observed_new_fans > 0.0 {
            durable_fans / observed_new_fans
        } else {
            0.0
        };
        // Incremental durable fans = incremental × durability ratio.
        // This assumes the durability ratio is the same for incremental and
        // organic fans — a reasonable approximation until we have experiment
        // data to distinguish them (Phase 5).
        let incremental_durable_fans = incremental_fans * durability_ratio;
        Self {
            observed_new_fans,
            counterfactual_fans,
            incremental_fans,
            durable_fans,
            incremental_durable_fans,
            pre_action_daily_rate,
            window_days,
        }
    }

    /// Returns true if this action produced any incremental fans.
    #[must_use]
    pub fn has_incremental_effect(self) -> bool {
        self.incremental_fans > 0.0
    }

    /// Returns the incremental rate (incremental fans per day).
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
    #[must_use]
    pub fn uplift_ratio(self) -> f64 {
        if self.counterfactual_fans <= 0.0 {
            return if self.observed_new_fans > 0.0 {
                f64::INFINITY
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
        assert_eq!(metric.incremental_fans, 0.0);
        assert_eq!(metric.incremental_durable_fans, 0.0);
        assert!(!metric.has_incremental_effect());
    }

    #[test]
    fn observed_equals_counterfactual_gives_zero_incremental() {
        // Pre-action rate: 1 fan/day → counterfactual = 14 fans in 14 days.
        // Observed: 14 fans → no incremental effect.
        let metric = NorthStarMetric::from_measurements(14.0, 10.0, 1.0, 14);
        assert_eq!(metric.counterfactual_fans, 14.0);
        assert_eq!(metric.incremental_fans, 0.0);
        assert!(!metric.has_incremental_effect());
    }

    #[test]
    fn observed_above_counterfactual_gives_positive_incremental() {
        // Pre-action rate: 1 fan/day → counterfactual = 14.
        // Observed: 20 fans → incremental = 6.
        let metric = NorthStarMetric::from_measurements(20.0, 15.0, 1.0, 14);
        assert_eq!(metric.counterfactual_fans, 14.0);
        assert!((metric.incremental_fans - 6.0).abs() < 0.01);
        assert!(metric.has_incremental_effect());
    }

    #[test]
    fn observed_below_counterfactual_clamps_to_zero() {
        // Pre-action rate: 2 fans/day → counterfactual = 28.
        // Observed: 10 fans → incremental = max(0, 10-28) = 0.
        let metric = NorthStarMetric::from_measurements(10.0, 5.0, 2.0, 14);
        assert_eq!(metric.counterfactual_fans, 28.0);
        assert_eq!(metric.incremental_fans, 0.0);
        assert!(!metric.has_incremental_effect());
    }

    #[test]
    fn durability_ratio_applies_to_incremental() {
        // 20 observed, 10 durable → 50% durability.
        // Incremental = 6 → incremental durable = 3.
        let metric = NorthStarMetric::from_measurements(20.0, 10.0, 1.0, 14);
        assert!((metric.incremental_durable_fans - 3.0).abs() < 0.01);
    }

    #[test]
    fn zero_observed_gives_zero_durable_incremental() {
        let metric = NorthStarMetric::from_measurements(0.0, 0.0, 1.0, 14);
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
    fn uplift_ratio_infinity_when_counterfactual_zero() {
        let metric = NorthStarMetric::from_measurements(5.0, 3.0, 0.0, 14);
        assert!(metric.uplift_ratio().is_infinite());
    }

    #[test]
    fn incremental_daily_rate_normalizes_by_window() {
        let metric = NorthStarMetric::from_measurements(20.0, 15.0, 1.0, 14);
        assert!((metric.incremental_daily_rate() - (6.0 / 14.0)).abs() < 0.01);
    }

    #[test]
    fn zero_window_does_not_divide_by_zero() {
        let metric = NorthStarMetric::from_measurements(10.0, 5.0, 1.0, 0);
        assert_eq!(metric.incremental_daily_rate(), 0.0);
    }
}
