//! Change-point detection for regime shifts in fan-growth time-series data.
//!
//! Ported from Kern's `brain::change_point`, which is the same author's engine
//! applied to capital. The CUSUM algorithm is domain-agnostic — it detects
//! shifts in any time series. The domain adaptation is in what we feed it:
//! daily fan counts, signal-install rates, and engagement metrics.
//!
//! # Why this matters for fan growth
//!
//! Fan growth is not smooth. A viral moment, a tour announcement, a platform
//! algorithm change, or audience fatigue can shift the growth rate abruptly.
//! The brain's self-assessment (`self_assessment.rs`) uses a proportional
//! threshold over a 30-day window — it detects slow trends but misses sudden
//! shifts. CUSUM catches the sudden ones:
//!
//! - **Upward shift** (viral moment): the brain should explore harder —
//!   the current channels are suddenly working, and the brain should
//!   capitalize on the momentum.
//! - **Downward shift** (algorithm change, audience fatigue): the brain
//!   should explore new channels — the current ones stopped working.
//!
//! # Algorithm
//!
//! CUSUM (Cumulative Sum) maintains two cumulative sums — one for upward
//! shifts (S_h) and one for downward shifts (S_l):
//!
//! - `S_h = max(0, S_h_prev + (value - mean - drift))`
//! - `S_l = max(0, S_l_prev + (mean - value - drift))`
//!
//! When `S_h` exceeds the threshold, an upward change point is detected
//! and `S_h` is reset to zero. When `S_l` exceeds the threshold, a
//! downward change point is detected and `S_l` is reset. The `drift`
//! parameter controls the allowed noise level — small fluctuations
//! around the mean don't accumulate in the cumulative sum.
//!
//! # Configuration
//!
//! - `threshold` controls sensitivity: lower values detect smaller
//!   shifts but produce more false positives.
//! - `drift` controls the noise band: higher values tolerate larger
//!   fluctuations without accumulating, at the cost of slower detection.
//!
//! For fan growth, the defaults are tuned for daily fan counts:
//! - `threshold = 10.0` (10 fans above the baseline triggers detection)
//! - `drift = 2.0` (2 fans of noise is tolerated per observation)

use serde::{Deserialize, Serialize};

/// Direction of a detected regime shift.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeDirection {
    /// The metric shifted upward (e.g. fan growth rate increasing — viral moment).
    Upward,
    /// The metric shifted downward (e.g. fan growth rate dropping — algorithm change).
    Downward,
}

impl ChangeDirection {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Upward => "upward",
            Self::Downward => "downward",
        }
    }

    #[must_use]
    pub const fn is_upward(self) -> bool {
        matches!(self, Self::Upward)
    }

    #[must_use]
    pub const fn is_downward(self) -> bool {
        matches!(self, Self::Downward)
    }
}

/// One detected change point in a time series.
///
/// All numeric fields are in the same units as the fed observations
/// (fan counts, signal-install rates, etc.).
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct ChangePoint {
    /// Observation index (zero-based count) at which the change point was detected.
    pub timestamp: usize,
    /// Direction of the shift.
    pub direction: ChangeDirection,
    /// Magnitude of the accumulated deviation — the CUSUM statistic that
    /// exceeded the threshold. Larger values indicate a more sustained shift.
    pub magnitude: f64,
    /// Running mean of observations before the shift (the baseline the
    /// detector was comparing against).
    pub pre_mean: f64,
    /// The observation value that triggered the detection.
    pub post_mean: f64,
}

impl ChangePoint {
    /// Absolute shift size: `|post_mean - pre_mean|`.
    #[must_use]
    pub fn shift_size(&self) -> f64 {
        (self.post_mean - self.pre_mean).abs()
    }
}

/// CUSUM-based change-point detector.
///
/// Maintains running cumulative sum statistics and fires when the sum
/// exceeds a configurable threshold. Feed it observations one at a time
/// via [`observe`](Self::observe); each call returns `Some(ChangePoint)`
/// if a regime shift was detected on that observation.
///
/// # Example
///
/// ```
/// use crowdrelay_brain::change_point::ChangePointDetector;
///
/// let mut det = ChangePointDetector::new(10.0, 2.0);
/// // Stable period — no detections.
/// for v in [5.0, 5.0, 5.0, 5.0] {
///     assert!(det.observe(v).is_none());
/// }
/// // Sudden upward shift.
/// let cp = det.observe(20.0).expect("shift should be detected");
/// assert!(cp.direction.is_upward());
/// ```
pub struct ChangePointDetector {
    /// Detection threshold — CUSUM must exceed this to fire.
    threshold: f64,
    /// Allowed drift / noise band around the mean.
    drift: f64,
    /// Upward CUSUM statistic.
    s_h: f64,
    /// Downward CUSUM statistic.
    s_l: f64,
    /// Number of observations seen.
    count: usize,
    /// Sum of all observations (for running mean).
    running_sum: f64,
    /// Frozen baseline mean from the pre-change regime. The CUSUM
    /// measures deviation from this, not from the running mean of all
    /// observations (which gets contaminated by post-change data).
    /// Updated to the post-change mean when a change point fires.
    baseline_mean: f64,
    /// Number of observations used to establish the baseline mean.
    baseline_count: usize,
    /// Detected change points, oldest first.
    change_points: Vec<ChangePoint>,
}

impl ChangePointDetector {
    /// Creates a new detector with the given threshold and drift.
    ///
    /// - `threshold`: how large the cumulative sum must grow before a
    ///   change point is declared. Lower = more sensitive.
    /// - `drift`: the allowed noise band. Observations within `drift` of
    ///   the running mean don't accumulate in the CUSUM. Higher = more
    ///   tolerant of small fluctuations.
    ///
    /// Both values should be non-negative and finite. A `threshold` of
    /// zero or less will fire on every observation that deviates beyond
    /// the drift band.
    #[must_use]
    pub fn new(threshold: f64, drift: f64) -> Self {
        Self {
            threshold,
            drift,
            s_h: 0.0,
            s_l: 0.0,
            count: 0,
            running_sum: 0.0,
            baseline_mean: 0.0,
            baseline_count: 0,
            change_points: Vec::new(),
        }
    }

    /// Returns the detection threshold.
    #[must_use]
    pub const fn threshold(&self) -> f64 {
        self.threshold
    }

    /// Returns the drift parameter.
    #[must_use]
    pub const fn drift(&self) -> f64 {
        self.drift
    }

    /// Returns the number of observations seen so far.
    #[must_use]
    pub const fn count(&self) -> usize {
        self.count
    }

    /// Returns the current upward CUSUM statistic.
    #[must_use]
    pub const fn s_h(&self) -> f64 {
        self.s_h
    }

    /// Returns the current downward CUSUM statistic.
    #[must_use]
    pub const fn s_l(&self) -> f64 {
        self.s_l
    }

    /// Returns the running mean of all observations, or `0.0` if none.
    #[must_use]
    pub fn mean(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.running_sum / self.count as f64
        }
    }

    /// Adds one observation and returns a [`ChangePoint`] if a shift is detected.
    ///
    /// The CUSUM measures deviation from a frozen baseline mean (the mean
    /// of observations in the pre-change regime). When a change point fires,
    /// the baseline is re-established to the post-change value, so the
    /// detector can find subsequent shifts without contamination from the
    /// old regime.
    ///
    /// The first observation establishes the baseline and never triggers
    /// a detection (there is no prior mean to deviate from).
    pub fn observe(&mut self, value: f64) -> Option<ChangePoint> {
        // First observation establishes the baseline — no change point possible.
        if self.count == 0 {
            self.running_sum = value;
            self.count = 1;
            self.baseline_mean = value;
            self.baseline_count = 1;
            return None;
        }

        // Use the frozen baseline mean, not the running mean of all
        // observations. The running mean gets contaminated by post-change
        // data, diluting the CUSUM and slowing/stalling subsequent
        // change-point detection.
        let mean = self.baseline_mean;

        // Update CUSUM statistics.
        self.s_h = (self.s_h + (value - mean - self.drift)).max(0.0);
        self.s_l = (self.s_l + (mean - value - self.drift)).max(0.0);

        // Record the observation.
        self.running_sum += value;
        self.count += 1;
        let timestamp = self.count - 1;

        // Check for upward shift (>= to catch exact-threshold cases).
        if self.s_h >= self.threshold {
            let cp = ChangePoint {
                timestamp,
                direction: ChangeDirection::Upward,
                magnitude: self.s_h,
                pre_mean: mean,
                post_mean: value,
            };
            // Reset both accumulators and re-establish the baseline
            // at the post-change value, so subsequent change points
            // are measured from the new regime, not the old one.
            self.s_h = 0.0;
            self.s_l = 0.0;
            self.baseline_mean = value;
            self.baseline_count = 1;
            self.change_points.push(cp);
            return Some(cp);
        }

        // Check for downward shift (>= to catch exact-threshold cases).
        if self.s_l >= self.threshold {
            let cp = ChangePoint {
                timestamp,
                direction: ChangeDirection::Downward,
                magnitude: self.s_l,
                pre_mean: mean,
                post_mean: value,
            };
            // Reset both accumulators and re-establish the baseline.
            self.s_h = 0.0;
            self.s_l = 0.0;
            self.baseline_mean = value;
            self.baseline_count = 1;
            self.change_points.push(cp);
            return Some(cp);
        }

        // No change point: absorb this observation into the baseline
        // (weighted). This lets the baseline track slow drift that
        // doesn't trigger a change point, while still detecting sudden
        // shifts. We use an exponential moving average with a slow
        // decay so the baseline is stable but not frozen.
        self.baseline_count += 1;
        let alpha = 1.0 / self.baseline_count as f64;
        self.baseline_mean = self.baseline_mean * (1.0 - alpha) + value * alpha;

        None
    }

    /// Resets all CUSUM statistics, observation count, running mean, and
    /// detected change points. The detector returns to its initial state
    /// (threshold and drift are preserved).
    pub fn reset(&mut self) {
        self.s_h = 0.0;
        self.s_l = 0.0;
        self.count = 0;
        self.running_sum = 0.0;
        self.baseline_mean = 0.0;
        self.baseline_count = 0;
        self.change_points.clear();
    }

    /// Returns all change points detected so far, oldest first.
    #[must_use]
    pub fn recent_change_points(&self) -> &[ChangePoint] {
        &self.change_points
    }

    /// Returns the most recent change point, if any.
    #[must_use]
    pub fn last_change_point(&self) -> Option<&ChangePoint> {
        self.change_points.last()
    }
}

impl Default for ChangePointDetector {
    fn default() -> Self {
        // threshold = 10, drift = 2 — tuned for daily fan counts.
        // A 10-fan cumulative deviation from the baseline triggers
        // detection; 2 fans of noise per observation is tolerated.
        // These are conservative defaults for a young system with
        // small daily counts. Operators can tune via the constructor.
        Self::new(10.0, 2.0)
    }
}

/// Runs CUSUM change-point detection on a fan-growth time series and
/// returns all detected change points. This is a stateless convenience
/// function — it creates a detector, feeds all observations, and returns
/// the results.
///
/// # Parameters
/// - `series`: time series of daily fan counts, oldest first.
/// - `threshold`: CUSUM detection threshold in fan counts.
/// - `drift`: allowed noise band in fan counts.
#[must_use]
pub fn detect_fan_growth_shifts(series: &[f64], threshold: f64, drift: f64) -> Vec<ChangePoint> {
    if series.len() < 2 {
        return Vec::new();
    }
    let mut detector = ChangePointDetector::new(threshold, drift);
    let mut shifts = Vec::new();
    for &value in series {
        if let Some(cp) = detector.observe(value) {
            shifts.push(cp);
        }
    }
    shifts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_as_str() {
        assert_eq!(ChangeDirection::Upward.as_str(), "upward");
        assert_eq!(ChangeDirection::Downward.as_str(), "downward");
    }

    #[test]
    fn direction_predicates() {
        assert!(ChangeDirection::Upward.is_upward());
        assert!(!ChangeDirection::Upward.is_downward());
        assert!(ChangeDirection::Downward.is_downward());
        assert!(!ChangeDirection::Downward.is_upward());
    }

    #[test]
    fn shift_size_is_absolute_difference() {
        let cp = ChangePoint {
            timestamp: 5,
            direction: ChangeDirection::Upward,
            magnitude: 95.0,
            pre_mean: 10.0,
            post_mean: 25.0,
        };
        assert_eq!(cp.shift_size(), 15.0);
    }

    #[test]
    fn stable_data_no_change_points() {
        let mut det = ChangePointDetector::new(10.0, 2.0);
        for _ in 0..20 {
            assert!(det.observe(5.0).is_none());
        }
        assert!(det.recent_change_points().is_empty());
    }

    #[test]
    fn noisy_data_within_drift_no_detection() {
        let mut det = ChangePointDetector::new(10.0, 5.0);
        let values = [5.0, 7.0, 3.0, 6.0, 4.0, 8.0, 2.0, 5.0];
        for v in values {
            assert!(det.observe(v).is_none(), "unexpected detection at {v}");
        }
        assert!(det.recent_change_points().is_empty());
    }

    #[test]
    fn upward_shift_detected() {
        let mut det = ChangePointDetector::new(10.0, 2.0);
        det.observe(5.0);
        det.observe(5.0);
        let cp = det.observe(20.0).expect("upward shift should be detected");
        assert_eq!(cp.direction, ChangeDirection::Upward);
        assert_eq!(cp.timestamp, 2);
        assert_eq!(cp.pre_mean, 5.0);
        assert_eq!(cp.post_mean, 20.0);
        // magnitude = s_h = max(0, 0 + (20 - 5 - 2)) = 13
        assert!((cp.magnitude - 13.0).abs() < f64::EPSILON);
    }

    #[test]
    fn downward_shift_detected() {
        let mut det = ChangePointDetector::new(10.0, 2.0);
        det.observe(20.0);
        det.observe(20.0);
        let cp = det.observe(5.0).expect("downward shift should be detected");
        assert_eq!(cp.direction, ChangeDirection::Downward);
        assert_eq!(cp.timestamp, 2);
        assert_eq!(cp.pre_mean, 20.0);
        assert_eq!(cp.post_mean, 5.0);
        // magnitude = s_l = max(0, 0 + (20 - 5 - 2)) = 13
        assert!((cp.magnitude - 13.0).abs() < f64::EPSILON);
    }

    #[test]
    fn first_observation_never_triggers() {
        let mut det = ChangePointDetector::new(1.0, 0.0);
        assert!(det.observe(1_000_000.0).is_none());
        assert_eq!(det.count(), 1);
        assert!(det.recent_change_points().is_empty());
    }

    #[test]
    fn reset_clears_all_state() {
        let mut det = ChangePointDetector::new(10.0, 2.0);
        det.observe(5.0);
        det.observe(5.0);
        det.observe(20.0); // triggers upward
        assert!(!det.recent_change_points().is_empty());
        assert_eq!(det.count(), 3);
        det.reset();
        assert!(det.recent_change_points().is_empty());
        assert_eq!(det.count(), 0);
        assert_eq!(det.mean(), 0.0);
        assert_eq!(det.s_h(), 0.0);
        assert_eq!(det.s_l(), 0.0);
    }

    #[test]
    fn reset_preserves_threshold_and_drift() {
        let mut det = ChangePointDetector::new(42.0, 7.0);
        det.reset();
        assert_eq!(det.threshold(), 42.0);
        assert_eq!(det.drift(), 7.0);
    }

    #[test]
    fn lower_threshold_is_more_sensitive() {
        // Values shift from 5 to 15 — a 10-unit jump.
        let values = [5.0, 5.0, 15.0, 15.0, 15.0];

        // High threshold — no detection.
        let mut strict = ChangePointDetector::new(50.0, 2.0);
        for v in values {
            strict.observe(v);
        }
        assert!(strict.recent_change_points().is_empty());

        // Low threshold — detection fires.
        let mut sensitive = ChangePointDetector::new(5.0, 2.0);
        for v in values {
            sensitive.observe(v);
        }
        assert!(!sensitive.recent_change_points().is_empty());
    }

    #[test]
    fn higher_drift_is_more_tolerant() {
        let values = [5.0, 5.0, 15.0, 15.0, 15.0];
        let mut tolerant = ChangePointDetector::new(10.0, 20.0);
        for v in values {
            tolerant.observe(v);
        }
        assert!(tolerant.recent_change_points().is_empty());
        let mut strict = ChangePointDetector::new(10.0, 2.0);
        for v in values {
            strict.observe(v);
        }
        assert!(!strict.recent_change_points().is_empty());
    }

    #[test]
    fn multiple_change_points_up_then_down() {
        let mut det = ChangePointDetector::new(10.0, 2.0);
        det.observe(5.0);
        det.observe(5.0);
        let up = det.observe(20.0).expect("upward shift");
        assert_eq!(up.direction, ChangeDirection::Upward);
        for _ in 0..20 {
            det.observe(20.0);
        }
        let down = det.observe(5.0);
        if down.is_none() {
            for _ in 0..5 {
                if det.observe(5.0).is_some() {
                    break;
                }
            }
        }
        let cps = det.recent_change_points();
        assert!(
            cps.iter().any(|c| c.direction == ChangeDirection::Downward),
            "should eventually detect a downward shift"
        );
        assert!(
            cps.iter().any(|c| c.direction == ChangeDirection::Upward),
            "should have detected the upward shift earlier"
        );
    }

    #[test]
    fn recent_change_points_returns_all_in_order() {
        let mut det = ChangePointDetector::new(5.0, 1.0);
        det.observe(5.0);
        det.observe(5.0);
        det.observe(20.0); // upward
        det.observe(20.0);
        for _ in 0..30 {
            det.observe(20.0);
        }
        det.observe(5.0); // downward (may need a few)
        for _ in 0..5 {
            if det.observe(5.0).is_some() {
                break;
            }
        }
        let cps = det.recent_change_points();
        assert!(cps.len() >= 2, "should have at least 2 change points");
        for i in 1..cps.len() {
            assert!(cps[i].timestamp > cps[i - 1].timestamp);
        }
    }

    #[test]
    fn mean_updates_with_observations() {
        let mut det = ChangePointDetector::new(10.0, 2.0);
        assert_eq!(det.mean(), 0.0);
        det.observe(5.0);
        assert_eq!(det.mean(), 5.0);
        det.observe(15.0);
        assert_eq!(det.mean(), 10.0);
    }

    #[test]
    fn count_tracks_observations() {
        let mut det = ChangePointDetector::new(10.0, 2.0);
        assert_eq!(det.count(), 0);
        det.observe(1.0);
        assert_eq!(det.count(), 1);
        det.observe(2.0);
        assert_eq!(det.count(), 2);
    }

    #[test]
    fn s_h_and_s_l_start_at_zero() {
        let det = ChangePointDetector::new(10.0, 2.0);
        assert_eq!(det.s_h(), 0.0);
        assert_eq!(det.s_l(), 0.0);
    }

    #[test]
    fn s_h_resets_after_upward_detection() {
        let mut det = ChangePointDetector::new(10.0, 2.0);
        det.observe(5.0);
        det.observe(5.0);
        det.observe(20.0); // triggers, s_h resets
        assert_eq!(det.s_h(), 0.0);
    }

    #[test]
    fn s_l_resets_after_downward_detection() {
        let mut det = ChangePointDetector::new(10.0, 2.0);
        det.observe(20.0);
        det.observe(20.0);
        det.observe(5.0); // triggers, s_l resets
        assert_eq!(det.s_l(), 0.0);
    }

    #[test]
    fn default_detector_has_moderate_sensitivity() {
        let det = ChangePointDetector::default();
        assert_eq!(det.threshold(), 10.0);
        assert_eq!(det.drift(), 2.0);
    }

    #[test]
    fn default_detector_catches_large_shift() {
        let mut det = ChangePointDetector::default();
        det.observe(5.0);
        det.observe(5.0);
        assert!(det.observe(50.0).is_some());
    }

    #[test]
    fn zero_drift_detects_any_sustained_deviation() {
        let mut det = ChangePointDetector::new(5.0, 0.0);
        det.observe(5.0);
        det.observe(5.0);
        assert!(det.observe(12.0).is_some());
    }

    #[test]
    fn negative_values_handled_correctly() {
        let mut det = ChangePointDetector::new(10.0, 2.0);
        det.observe(-5.0);
        det.observe(-5.0);
        let cp = det.observe(-20.0).expect("downward shift");
        assert_eq!(cp.direction, ChangeDirection::Downward);
        assert_eq!(cp.pre_mean, -5.0);
        assert_eq!(cp.post_mean, -20.0);
    }

    #[test]
    fn last_change_point_returns_most_recent() {
        let mut det = ChangePointDetector::new(5.0, 1.0);
        det.observe(5.0);
        det.observe(5.0);
        det.observe(20.0);
        let last = det.last_change_point().expect("should have a change point");
        assert_eq!(last.post_mean, 20.0);
    }

    #[test]
    fn detect_fan_growth_shifts_convenience_function() {
        let series = [5.0, 5.0, 5.0, 20.0, 20.0, 20.0];
        let shifts = detect_fan_growth_shifts(&series, 10.0, 2.0);
        assert!(!shifts.is_empty());
        assert_eq!(shifts[0].direction, ChangeDirection::Upward);
    }

    #[test]
    fn detect_fan_growth_shifts_empty_series() {
        let shifts = detect_fan_growth_shifts(&[], 10.0, 2.0);
        assert!(shifts.is_empty());
    }

    #[test]
    fn detect_fan_growth_shifts_single_observation() {
        let shifts = detect_fan_growth_shifts(&[5.0], 10.0, 2.0);
        assert!(shifts.is_empty());
    }

    #[test]
    fn change_point_serializes_and_deserializes() {
        let cp = ChangePoint {
            timestamp: 42,
            direction: ChangeDirection::Upward,
            magnitude: 13.0,
            pre_mean: 5.0,
            post_mean: 20.0,
        };
        let json = serde_json::to_string(&cp).expect("serialize");
        let back: ChangePoint = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(cp, back);
    }

    #[test]
    fn direction_serializes_snake_case() {
        let json = serde_json::to_string(&ChangeDirection::Upward).expect("serialize");
        assert_eq!(json, "\"upward\"");
        let json = serde_json::to_string(&ChangeDirection::Downward).expect("serialize");
        assert_eq!(json, "\"downward\"");
    }
}
