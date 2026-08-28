//! Change-point detection for the brain's time-series data.
//!
//! The brain ingests streaming metrics — fan arrival rates, engagement scores,
//! subreddit responsiveness — whose underlying distribution can shift abruptly
//! when the world changes: a viral post, a tour announcement, a ban wave. The
//! [`ChangePointDetector`] uses a CUSUM (Cumulative Sum) scheme to flag those
//! regime shifts in real time so the causal model and portfolio optimizer can
//! react instead of waiting for slow-moving EMAs to catch up.
//!
//! # Algorithm
//!
//! The detector maintains two running cumulative sums:
//!
//! - `S_h` tracks **positive** shifts (growth acceleration, responsiveness up).
//! - `S_l` tracks **negative** shifts (growth collapse, engagement drop).
//!
//! After subtracting a reference level (the mean of the warm-up sample), each
//! incoming observation updates both sums. When either sum exceeds the
//! configured `threshold`, a [`ChangePoint`] is emitted and the detector
//! resets so it can find the *next* regime change.

use serde::Serialize;
use time::OffsetDateTime;

/// Direction of a detected distribution shift.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum ChangeDirection {
    /// The underlying mean moved **up** — growth accelerated or responsiveness
    /// increased.
    Increase,
    /// The underlying mean moved **down** — growth collapsed or engagement
    /// dropped.
    Decrease,
}

/// A single detected change point in a streaming time series.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChangePoint {
    /// Wall-clock instant at which the change was detected.
    pub detected_at: OffsetDateTime,
    /// Estimated magnitude of the shift, expressed as the cumulative deviation
    /// that triggered the detection. Always positive; pair with
    /// [`ChangeDirection`] to know the sign.
    pub magnitude: f64,
    /// Whether the shift was an increase or a decrease.
    pub direction: ChangeDirection,
}

/// CUSUM change-point detector for streaming scalar metrics.
///
/// The detector first collects a small warm-up sample to estimate the baseline
/// mean of the series. Once warmed up it updates the positive (`S_h`) and
/// negative (`S_l`) cumulative sums on every [`update`](Self::update) call and
/// emits a [`ChangePoint`] the moment either sum crosses `threshold`.
///
/// ```
/// use crowdrelay_brain::ChangePointDetector;
///
/// let mut det = ChangePointDetector::with_warmup(5.0, 5);
/// // Feed a stable warm-up series around 10.0.
/// for v in [10.0, 10.1, 9.9, 10.0, 10.1] {
///     let _ = det.update(v);
/// }
/// // No change yet.
/// assert!(det.update(10.0).is_none());
/// // Sudden jump should trip the detector.
/// assert!(det.update(20.0).is_some());
/// ```
#[derive(Clone, Debug)]
pub struct ChangePointDetector {
    /// Cumulative-sum threshold. Either `S_h` or `S_l` must reach this value
    /// for a change point to fire.
    threshold: f64,
    /// Running positive CUSUM (detects upward shifts).
    s_h: f64,
    /// Running negative CUSUM (detects downward shifts).
    s_l: f64,
    /// Estimated baseline mean of the series.
    mean: f64,
    /// Number of warm-up observations collected so far.
    warmup_count: usize,
    /// Running sum of warm-up observations.
    warmup_sum: f64,
    /// Number of observations required before the detector arms itself.
    warmup_target: usize,
}

impl ChangePointDetector {
    /// Number of observations used to estimate the baseline mean before the
    /// CUSUM goes live.
    const WARMUP_DEFAULT: usize = 10;

    /// Creates a new detector that fires when either cumulative sum exceeds
    /// `threshold`.
    ///
    /// The threshold is expressed in the same units as the observed metric.
    /// Larger values make the detector less sensitive (fewer false positives,
    /// slower detection); smaller values make it more sensitive.
    #[must_use]
    pub fn new(threshold: f64) -> Self {
        Self::with_warmup(threshold, Self::WARMUP_DEFAULT)
    }

    /// Creates a detector with a custom warm-up sample size.
    ///
    /// During the warm-up phase the detector estimates the baseline mean and
    /// never emits change points. Once `warmup_size` observations have been
    /// ingested the CUSUM goes live.
    #[must_use]
    pub fn with_warmup(threshold: f64, warmup_size: usize) -> Self {
        Self {
            threshold: threshold.max(0.0),
            s_h: 0.0,
            s_l: 0.0,
            mean: 0.0,
            warmup_count: 0,
            warmup_sum: 0.0,
            warmup_target: warmup_size.max(1),
        }
    }

    /// Returns the configured detection threshold.
    #[must_use]
    pub fn threshold(&self) -> f64 {
        self.threshold
    }

    /// Returns `true` while the detector is still estimating its baseline mean.
    #[must_use]
    pub fn is_warming_up(&self) -> bool {
        self.warmup_count < self.warmup_target
    }

    /// Returns the current baseline mean estimate, or `0.0` during warm-up.
    #[must_use]
    pub fn mean(&self) -> f64 {
        self.mean
    }

    /// Ingests a new observation and returns a [`ChangePoint`] if a regime
    /// shift was detected.
    ///
    /// During warm-up the call only updates the baseline estimate and always
    /// returns `None`. Once armed, the observation updates `S_h` and `S_l`;
    /// if either crosses the threshold a change point is returned and the
    /// detector resets its cumulative sums (but keeps the baseline mean) so
    /// it can detect the *next* shift.
    #[must_use]
    pub fn update(&mut self, value: f64) -> Option<ChangePoint> {
        if self.is_warming_up() {
            self.warmup_sum += value;
            self.warmup_count += 1;
            if self.warmup_count == self.warmup_target {
                self.mean = self.warmup_sum / self.warmup_count as f64;
            }
            return None;
        }

        // Deviation from the baseline mean.
        let delta = value - self.mean;

        // Standard two-sided CUSUM: each sum accumulates deviations in its
        // direction but is floored at zero so stale drift decays away.
        self.s_h = (self.s_h + delta).max(0.0);
        self.s_l = (self.s_l - delta).max(0.0);

        if self.s_h > self.threshold {
            let cp = ChangePoint {
                detected_at: OffsetDateTime::now_utc(),
                magnitude: self.s_h,
                direction: ChangeDirection::Increase,
            };
            self.reset_sums();
            return Some(cp);
        }
        if self.s_l > self.threshold {
            let cp = ChangePoint {
                detected_at: OffsetDateTime::now_utc(),
                magnitude: self.s_l,
                direction: ChangeDirection::Decrease,
            };
            self.reset_sums();
            return Some(cp);
        }
        None
    }

    /// Fully resets the detector, clearing the baseline estimate and the
    /// cumulative sums. After a reset the detector re-enters warm-up.
    pub fn reset(&mut self) {
        self.s_h = 0.0;
        self.s_l = 0.0;
        self.mean = 0.0;
        self.warmup_count = 0;
        self.warmup_sum = 0.0;
    }

    /// Resets only the cumulative sums, keeping the baseline mean so the
    /// detector can immediately look for the next change.
    fn reset_sums(&mut self) {
        self.s_h = 0.0;
        self.s_l = 0.0;
    }
}

impl Default for ChangePointDetector {
    fn default() -> Self {
        Self::new(5.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `values` into the detector and returns the first detected change
    /// point, if any.
    fn first_change(detector: &mut ChangePointDetector, values: &[f64]) -> Option<ChangePoint> {
        values.iter().find_map(|&v| detector.update(v))
    }

    #[test]
    fn stable_series_detects_no_change() {
        let mut det = ChangePointDetector::new(5.0);
        // 30 observations tightly clustered around 10.0 — no shift.
        let series: Vec<f64> = (0..30)
            .map(|i| 10.0 + (i % 3) as f64 * 0.01 - 0.01)
            .collect();
        assert!(first_change(&mut det, &series).is_none());
    }

    #[test]
    fn sudden_increase_is_detected() {
        let mut det = ChangePointDetector::new(5.0);
        // Warm up around 10.0.
        for v in [10.0, 10.0, 10.0, 10.0, 10.0, 10.0, 10.0, 10.0, 10.0, 10.0] {
            assert!(det.update(v).is_none());
        }
        // Jump to 20.0 — delta = +10 every step should trip S_h quickly.
        let cp = first_change(&mut det, &[20.0, 20.0]);
        let cp = cp.expect("expected an increase change point");
        assert_eq!(cp.direction, ChangeDirection::Increase);
        assert!(cp.magnitude > 5.0);
    }

    #[test]
    fn sudden_decrease_is_detected() {
        let mut det = ChangePointDetector::new(5.0);
        for v in [10.0, 10.0, 10.0, 10.0, 10.0, 10.0, 10.0, 10.0, 10.0, 10.0] {
            assert!(det.update(v).is_none());
        }
        // Drop to 2.0 — delta = -8 every step should trip S_l quickly.
        let cp = first_change(&mut det, &[2.0, 2.0]);
        let cp = cp.expect("expected a decrease change point");
        assert_eq!(cp.direction, ChangeDirection::Decrease);
        assert!(cp.magnitude > 5.0);
    }

    #[test]
    fn gradual_drift_is_detected() {
        let mut det = ChangePointDetector::new(5.0);
        // Warm up around 10.0.
        for v in [10.0; 10] {
            let _ = det.update(v);
        }
        // Slowly drift up by 1.0 each step. The cumulative sum grows until it
        // crosses the threshold.
        let mut detected = None;
        for i in 1..=20 {
            if let Some(cp) = det.update(10.0 + i as f64) {
                detected = Some(cp);
                break;
            }
        }
        let cp = detected.expect("gradual drift should eventually trip the detector");
        assert_eq!(cp.direction, ChangeDirection::Increase);
    }

    #[test]
    fn threshold_sensitivity_controls_detection() {
        // Low threshold: should detect a mild shift.
        let mut sensitive = ChangePointDetector::new(1.0);
        for v in [10.0; 10] {
            let _ = sensitive.update(v);
        }
        assert!(first_change(&mut sensitive, &[12.0, 12.0]).is_some());

        // High threshold: the same mild shift should not trip it.
        let mut insensitive = ChangePointDetector::new(50.0);
        for v in [10.0; 10] {
            let _ = insensitive.update(v);
        }
        assert!(first_change(&mut insensitive, &[12.0, 12.0]).is_none());
    }

    #[test]
    fn warm_up_never_emits_change_points() {
        let mut det = ChangePointDetector::with_warmup(1.0, 5);
        // Even extreme values during warm-up are absorbed into the baseline.
        for v in [100.0, 100.0, 100.0, 100.0, 100.0] {
            assert!(det.update(v).is_none());
        }
        assert!(!det.is_warming_up());
        // Baseline is now ~100, so a value of 100 is not a change.
        assert!(det.update(100.0).is_none());
    }

    #[test]
    fn reset_clears_state() {
        let mut det = ChangePointDetector::new(5.0);
        for v in [10.0; 10] {
            let _ = det.update(v);
        }
        let _ = det.update(20.0);
        det.reset();
        assert!(det.is_warming_up());
        assert_eq!(det.mean(), 0.0);
    }

    #[test]
    fn detector_detects_multiple_changes() {
        let mut det = ChangePointDetector::new(5.0);
        for v in [10.0; 10] {
            let _ = det.update(v);
        }
        // First change: jump up.
        let first = first_change(&mut det, &[20.0, 20.0]).expect("first change");
        assert_eq!(first.direction, ChangeDirection::Increase);
        // The baseline mean is still 10, so dropping well below it produces
        // large negative deltas that should trip S_l.
        let second = first_change(&mut det, &[2.0, 2.0]).expect("second change");
        assert_eq!(second.direction, ChangeDirection::Decrease);
    }

    #[test]
    fn default_threshold_is_five() {
        let det = ChangePointDetector::default();
        assert!((det.threshold() - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn change_point_serializes() {
        let cp = ChangePoint {
            detected_at: OffsetDateTime::now_utc(),
            magnitude: 7.5,
            direction: ChangeDirection::Increase,
        };
        let json = serde_json::to_string(&cp).expect("serialize");
        assert!(json.contains("Increase"));
        assert!(json.contains("7.5"));
    }

    #[test]
    fn negative_threshold_is_clamped_to_zero() {
        let det = ChangePointDetector::new(-1.0);
        assert!((det.threshold() - 0.0).abs() < f64::EPSILON);
    }
}
