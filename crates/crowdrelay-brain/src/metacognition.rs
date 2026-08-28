//! Metacognition — the brain thinks about its own thinking.
//!
//! The brain monitors its own performance over time, detects when it is stuck
//! (stagnant) or regressing, and adjusts its exploration/exploitation balance
//! accordingly. When the North Star metric stops improving, the brain boosts
//! exploration to break out of the local optimum; when it is actively
//! regressing, it boosts exploration even more aggressively.
//!
//! # How it works
//!
//! - [`MetacognitionMonitor`] keeps a rolling window of recent North Star
//!   metric values (one per autopilot cycle).
//! - [`MetacognitionMonitor::is_stagnant`] compares the average of the most
//!   recent `window_size` cycles against the average of the preceding
//!   `window_size` cycles. If the improvement is below
//!   `stagnation_threshold`, the brain is stagnant.
//! - [`MetacognitionMonitor::is_regressing`] fires when the recent average is
//!   significantly *lower* than the previous average (by
//!   `stagnation_threshold`).
//! - [`MetacognitionMonitor::recommended_exploration_weight`] translates the
//!   current state into an exploration weight for EFE scoring: stagnant gets a
//!   boost, regressing gets a double boost, improving stays at the baseline.

use serde::Serialize;

/// The default exploration weight, matching [`crate::efe::EfeWeights::default`].
const BASE_EXPLORATION_WEIGHT: f64 = 0.3;

/// The metacognitive state of the brain.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum MetacognitiveState {
    /// The North Star metric is trending upward.
    Improving,
    /// The North Star metric is flat — improvement below the stagnation
    /// threshold.
    Stagnant,
    /// The North Star metric is trending downward.
    Regressing,
    /// Not enough cycles have been recorded to determine a trend.
    InsufficientData,
}

/// Monitors the brain's own performance and recommends exploration adjustments.
///
/// The monitor keeps a rolling window of recent North Star metric values. By
/// comparing the most recent window against the preceding window, it can
/// detect stagnation (flat performance) and regression (declining
/// performance), and recommend a higher exploration weight to break out of
/// local optima.
#[derive(Clone, Debug, Serialize)]
pub struct MetacognitionMonitor {
    /// Recent North Star metric values, oldest first.
    pub recent_performance: Vec<f64>,
    /// How many recent cycles to consider when computing averages.
    pub window_size: usize,
    /// If the average improvement between windows is below this, the brain is
    /// stagnant.
    pub stagnation_threshold: f64,
    /// How much to boost exploration when the brain is stagnant.
    pub exploration_boost: f64,
}

impl Default for MetacognitionMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl MetacognitionMonitor {
    /// Creates a new monitor with sensible defaults.
    #[must_use]
    pub fn new() -> Self {
        Self {
            recent_performance: Vec::new(),
            window_size: 10,
            stagnation_threshold: 0.01,
            exploration_boost: 0.2,
        }
    }

    /// Records a North Star metric value for the most recent autopilot cycle.
    ///
    /// The value is pushed onto [`recent_performance`](Self::recent_performance)
    /// and the buffer is trimmed to `2 * window_size` entries (FIFO), which is
    /// enough to compare the recent window against the preceding window.
    pub fn record_cycle(&mut self, north_star_value: f64) {
        self.recent_performance.push(north_star_value);
        let max_len = self.window_size.saturating_mul(2);
        if self.recent_performance.len() > max_len {
            let drop = self.recent_performance.len() - max_len;
            self.recent_performance.drain(0..drop);
        }
    }

    /// Returns the average of the last `n` values in the buffer, or `None` if
    /// fewer than `n` values are available.
    fn window_average(&self, n: usize) -> Option<f64> {
        if n == 0 || self.recent_performance.len() < n {
            return None;
        }
        let start = self.recent_performance.len() - n;
        let sum: f64 = self.recent_performance[start..].iter().sum();
        Some(sum / n as f64)
    }

    /// Returns `true` if the brain is stagnant — the recent window's average
    /// is within `stagnation_threshold` of the preceding window's average.
    ///
    /// Returns `false` if fewer than `2 * window_size` cycles have been
    /// recorded (insufficient data).
    #[must_use]
    pub fn is_stagnant(&self) -> bool {
        let Some(recent) = self.window_average(self.window_size) else {
            return false;
        };
        let len = self.recent_performance.len();
        if len < self.window_size.saturating_mul(2) {
            return false;
        }
        let prev_start = len - self.window_size.saturating_mul(2);
        let prev_end = len - self.window_size;
        let prev_sum: f64 = self.recent_performance[prev_start..prev_end].iter().sum();
        let prev_avg = prev_sum / self.window_size as f64;
        let improvement = recent - prev_avg;
        improvement.abs() < self.stagnation_threshold
    }

    /// Returns `true` if the brain is regressing — the recent window's average
    /// is lower than the preceding window's average by more than
    /// `stagnation_threshold`.
    ///
    /// Returns `false` if fewer than `2 * window_size` cycles have been
    /// recorded (insufficient data).
    #[must_use]
    pub fn is_regressing(&self) -> bool {
        let Some(recent) = self.window_average(self.window_size) else {
            return false;
        };
        let len = self.recent_performance.len();
        if len < self.window_size.saturating_mul(2) {
            return false;
        }
        let prev_start = len - self.window_size.saturating_mul(2);
        let prev_end = len - self.window_size;
        let prev_sum: f64 = self.recent_performance[prev_start..prev_end].iter().sum();
        let prev_avg = prev_sum / self.window_size as f64;
        recent < prev_avg - self.stagnation_threshold
    }

    /// Returns `true` if the brain is improving — the recent window's average
    /// is higher than the preceding window's average by more than
    /// `stagnation_threshold`.
    ///
    /// Returns `false` if fewer than `2 * window_size` cycles have been
    /// recorded (insufficient data).
    #[must_use]
    pub fn is_improving(&self) -> bool {
        let Some(recent) = self.window_average(self.window_size) else {
            return false;
        };
        let len = self.recent_performance.len();
        if len < self.window_size.saturating_mul(2) {
            return false;
        }
        let prev_start = len - self.window_size.saturating_mul(2);
        let prev_end = len - self.window_size;
        let prev_sum: f64 = self.recent_performance[prev_start..prev_end].iter().sum();
        let prev_avg = prev_sum / self.window_size as f64;
        recent > prev_avg + self.stagnation_threshold
    }

    /// Recommends an exploration weight for EFE scoring based on the current
    /// metacognitive state.
    ///
    /// - **Improving**: baseline (`0.3`) — no boost needed, keep exploiting.
    /// - **Stagnant**: baseline + `exploration_boost` — nudge exploration.
    /// - **Regressing**: baseline + `2 * exploration_boost` — aggressive
    ///   exploration to escape the regression.
    /// - **Insufficient data**: baseline.
    #[must_use]
    pub fn recommended_exploration_weight(&self) -> f64 {
        if self.is_regressing() {
            BASE_EXPLORATION_WEIGHT + 2.0 * self.exploration_boost
        } else if self.is_stagnant() {
            BASE_EXPLORATION_WEIGHT + self.exploration_boost
        } else {
            BASE_EXPLORATION_WEIGHT
        }
    }

    /// Computes the linear regression slope of `recent_performance`.
    ///
    /// A positive slope means the North Star metric is improving, a negative
    /// slope means it is regressing, and a near-zero slope means it is
    /// stagnant. Returns `0.0` if there are fewer than 2 data points.
    #[must_use]
    pub fn performance_trend(&self) -> f64 {
        let n = self.recent_performance.len();
        if n < 2 {
            return 0.0;
        }
        let n_f = n as f64;
        let sum_x: f64 = (0..n).map(|i| i as f64).sum();
        let sum_y: f64 = self.recent_performance.iter().sum();
        let sum_xy: f64 = self
            .recent_performance
            .iter()
            .enumerate()
            .map(|(i, &y)| i as f64 * y)
            .sum();
        let sum_x2: f64 = (0..n).map(|i| (i as f64).powi(2)).sum();
        let denominator = n_f * sum_x2 - sum_x * sum_x;
        if denominator.abs() < f64::EPSILON {
            return 0.0;
        }
        (n_f * sum_xy - sum_x * sum_y) / denominator
    }

    /// Returns the current metacognitive state, combining the stagnation,
    /// regression, and improvement checks.
    ///
    /// If there is insufficient data (fewer than `2 * window_size` cycles),
    /// returns [`MetacognitiveState::InsufficientData`].
    #[must_use]
    pub fn current_state(&self) -> MetacognitiveState {
        if self.recent_performance.len() < self.window_size.saturating_mul(2) {
            return MetacognitiveState::InsufficientData;
        }
        if self.is_regressing() {
            MetacognitiveState::Regressing
        } else if self.is_stagnant() {
            MetacognitiveState::Stagnant
        } else {
            MetacognitiveState::Improving
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_monitor_is_insufficient_data() {
        let monitor = MetacognitionMonitor::new();
        assert!(!monitor.is_stagnant());
        assert!(!monitor.is_regressing());
        assert!(!monitor.is_improving());
        assert_eq!(
            monitor.current_state(),
            MetacognitiveState::InsufficientData
        );
        assert!((monitor.recommended_exploration_weight() - 0.3).abs() < 0.001);
        assert!((monitor.performance_trend()).abs() < 0.001);
    }

    #[test]
    fn record_cycle_trims_to_window() {
        let mut monitor = MetacognitionMonitor::new();
        monitor.window_size = 3;
        for v in 0..10 {
            monitor.record_cycle(v as f64);
        }
        // 2 * window_size = 6 entries kept.
        assert_eq!(monitor.recent_performance.len(), 6);
        // FIFO: the oldest kept value is 4.
        assert!((monitor.recent_performance[0] - 4.0).abs() < 0.001);
        assert!((monitor.recent_performance[5] - 9.0).abs() < 0.001);
    }

    #[test]
    fn improving_performance_detected() {
        let mut monitor = MetacognitionMonitor::new();
        monitor.window_size = 3;
        monitor.stagnation_threshold = 0.5;
        // Previous window: low values.
        for v in [1.0, 2.0, 3.0] {
            monitor.record_cycle(v);
        }
        // Recent window: clearly higher.
        for v in [10.0, 11.0, 12.0] {
            monitor.record_cycle(v);
        }
        assert!(monitor.is_improving());
        assert!(!monitor.is_stagnant());
        assert!(!monitor.is_regressing());
        assert_eq!(monitor.current_state(), MetacognitiveState::Improving);
    }

    #[test]
    fn improving_performance_keeps_baseline_exploration() {
        let mut monitor = MetacognitionMonitor::new();
        monitor.window_size = 3;
        monitor.stagnation_threshold = 0.5;
        for v in [1.0, 2.0, 3.0] {
            monitor.record_cycle(v);
        }
        for v in [10.0, 11.0, 12.0] {
            monitor.record_cycle(v);
        }
        assert!((monitor.recommended_exploration_weight() - 0.3).abs() < 0.001);
    }

    #[test]
    fn stagnant_performance_detected() {
        let mut monitor = MetacognitionMonitor::new();
        monitor.window_size = 3;
        monitor.stagnation_threshold = 1.0;
        // Previous and recent windows are nearly identical.
        for v in [10.0, 10.0, 10.0] {
            monitor.record_cycle(v);
        }
        for v in [10.2, 10.1, 10.0] {
            monitor.record_cycle(v);
        }
        assert!(monitor.is_stagnant());
        assert!(!monitor.is_improving());
        assert!(!monitor.is_regressing());
        assert_eq!(monitor.current_state(), MetacognitiveState::Stagnant);
    }

    #[test]
    fn stagnant_performance_boosts_exploration() {
        let mut monitor = MetacognitionMonitor::new();
        monitor.window_size = 3;
        monitor.stagnation_threshold = 1.0;
        monitor.exploration_boost = 0.2;
        for v in [10.0, 10.0, 10.0] {
            monitor.record_cycle(v);
        }
        for v in [10.2, 10.1, 10.0] {
            monitor.record_cycle(v);
        }
        // base (0.3) + boost (0.2) = 0.5
        assert!((monitor.recommended_exploration_weight() - 0.5).abs() < 0.001);
    }

    #[test]
    fn regressing_performance_detected() {
        let mut monitor = MetacognitionMonitor::new();
        monitor.window_size = 3;
        monitor.stagnation_threshold = 0.5;
        // Previous window: high values.
        for v in [10.0, 11.0, 12.0] {
            monitor.record_cycle(v);
        }
        // Recent window: clearly lower.
        for v in [1.0, 2.0, 3.0] {
            monitor.record_cycle(v);
        }
        assert!(monitor.is_regressing());
        assert!(!monitor.is_stagnant());
        assert!(!monitor.is_improving());
        assert_eq!(monitor.current_state(), MetacognitiveState::Regressing);
    }

    #[test]
    fn regressing_performance_doubles_exploration_boost() {
        let mut monitor = MetacognitionMonitor::new();
        monitor.window_size = 3;
        monitor.stagnation_threshold = 0.5;
        monitor.exploration_boost = 0.2;
        for v in [10.0, 11.0, 12.0] {
            monitor.record_cycle(v);
        }
        for v in [1.0, 2.0, 3.0] {
            monitor.record_cycle(v);
        }
        // base (0.3) + 2 * boost (0.4) = 0.7
        assert!((monitor.recommended_exploration_weight() - 0.7).abs() < 0.001);
    }

    #[test]
    fn insufficient_data_when_fewer_than_two_windows() {
        let mut monitor = MetacognitionMonitor::new();
        monitor.window_size = 5;
        for v in [1.0, 2.0, 3.0, 4.0] {
            monitor.record_cycle(v);
        }
        assert_eq!(
            monitor.current_state(),
            MetacognitiveState::InsufficientData
        );
        assert!(!monitor.is_stagnant());
        assert!(!monitor.is_regressing());
    }

    #[test]
    fn performance_trend_positive_for_increasing_values() {
        let mut monitor = MetacognitionMonitor::new();
        for v in [1.0, 2.0, 3.0, 4.0, 5.0] {
            monitor.record_cycle(v);
        }
        let trend = monitor.performance_trend();
        assert!(trend > 0.0, "increasing values should have positive trend");
        // Slope of y = x + 1 is exactly 1.0.
        assert!((trend - 1.0).abs() < 0.001);
    }

    #[test]
    fn performance_trend_negative_for_decreasing_values() {
        let mut monitor = MetacognitionMonitor::new();
        for v in [5.0, 4.0, 3.0, 2.0, 1.0] {
            monitor.record_cycle(v);
        }
        let trend = monitor.performance_trend();
        assert!(trend < 0.0, "decreasing values should have negative trend");
        assert!((trend + 1.0).abs() < 0.001);
    }

    #[test]
    fn performance_trend_near_zero_for_flat_values() {
        let mut monitor = MetacognitionMonitor::new();
        for _ in 0..5 {
            monitor.record_cycle(7.0);
        }
        let trend = monitor.performance_trend();
        assert!(trend.abs() < 0.001, "flat values should have zero trend");
    }

    #[test]
    fn performance_trend_zero_for_single_value() {
        let mut monitor = MetacognitionMonitor::new();
        monitor.record_cycle(42.0);
        assert!((monitor.performance_trend()).abs() < 0.001);
    }

    #[test]
    fn current_state_prefers_regressing_over_stagnant() {
        // When both could be arguable, regression takes priority.
        let mut monitor = MetacognitionMonitor::new();
        monitor.window_size = 2;
        monitor.stagnation_threshold = 0.1;
        for v in [10.0, 10.0] {
            monitor.record_cycle(v);
        }
        for v in [5.0, 5.0] {
            monitor.record_cycle(v);
        }
        assert_eq!(monitor.current_state(), MetacognitiveState::Regressing);
    }

    #[test]
    fn serde_serializes_state() {
        let states = [
            MetacognitiveState::Improving,
            MetacognitiveState::Stagnant,
            MetacognitiveState::Regressing,
            MetacognitiveState::InsufficientData,
        ];
        for state in states {
            let json = serde_json::to_string(&state).expect("state should serialize");
            assert!(!json.is_empty());
        }
    }

    #[test]
    fn serde_serializes_monitor() {
        let mut monitor = MetacognitionMonitor::new();
        monitor.record_cycle(1.0);
        monitor.record_cycle(2.0);
        let json = serde_json::to_string(&monitor).expect("monitor should serialize");
        assert!(json.contains("recent_performance"));
        assert!(json.contains("window_size"));
    }
}
