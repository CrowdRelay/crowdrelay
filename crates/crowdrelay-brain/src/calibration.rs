//! Predictive self-criticism and calibration.
//!
//! The brain makes predictions about dispatch outcomes (expected fans,
//! treatment effects, etc.) and then observes the actual results. This module
//! tracks the accuracy of those predictions and uses the calibration data to
//! self-criticize: if the brain's predictions are systematically biased, it
//! can correct them before making decisions.
//!
//! # Calibration
//!
//! A well-calibrated predictor has the property: "when I say 80% confidence,
//! I'm right 80% of the time." For continuous predictions, calibration means
//! the predicted distribution matches the observed distribution — the
//! predicted 90% credible interval contains 90% of observations.
//!
//! # Self-criticism
//!
//! Self-criticism is the brain's ability to look at its own prediction
//! history and say: "I've been overestimating by 30%, so I should discount
//! my current predictions by that factor." This is implemented via:
//!
//! - **Bias tracking**: the running mean of (predicted - observed), which
//!   measures systematic over/under-prediction.
//! - **Calibration slope**: the regression slope of observed on predicted,
//!   which measures whether the brain's confidence scales correctly with
//!   the magnitude of the prediction.
//! - **Reliability diagram**: bucketed predictions vs. observations, which
//!   shows where the brain is over/under-confident.
//!
//! # Integration
//!
//! The calibration data feeds back into the EFE scorer: before scoring a
//! candidate, the brain applies the bias correction and calibration slope
//! to the predicted outcome. This makes the EFE score more honest — it
//! reflects what the brain *actually* expects to happen, not what its
//! optimistic model predicts.

use serde::{Deserialize, Serialize};

/// A single prediction-observation pair, recorded for calibration analysis.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PredictionRecord {
    /// The predicted expected value (e.g. expected fans).
    pub predicted: f64,
    /// The predicted standard deviation (uncertainty).
    pub predicted_std: f64,
    /// The observed outcome.
    pub observed: f64,
    /// The template that was dispatched.
    pub template_id: String,
    /// The target/audience (e.g. subreddit). Used for hierarchical
    /// calibration at progressively more specific levels.
    #[serde(default)]
    pub target: Option<String>,
    /// The channel (reddit, instagram, etc.).
    #[serde(default)]
    pub channel: Option<String>,
    /// The estimation regime used for this prediction.
    #[serde(default)]
    pub estimation_regime: Option<String>,
    /// The evidence quality supporting this prediction.
    #[serde(default)]
    pub evidence_quality: Option<String>,
}

/// The result of a calibration analysis.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CalibrationReport {
    /// The mean bias: average (predicted - observed). Positive = over-predicting.
    pub bias: f64,
    /// The mean absolute error: average |predicted - observed|.
    pub mae: f64,
    /// The root mean squared error: sqrt(mean((predicted - observed)²)).
    pub rmse: f64,
    /// The calibration slope: regression slope of observed on predicted.
    /// 1.0 = perfectly calibrated. <1.0 = over-confident. >1.0 = under-confident.
    pub calibration_slope: f64,
    /// The calibration intercept: regression intercept of observed on predicted.
    pub calibration_intercept: f64,
    /// The number of predictions analyzed.
    pub n: usize,
    /// The reliability diagram: bucketed predictions vs. observations.
    pub reliability: Vec<ReliabilityBucket>,
}

/// One bucket in the reliability diagram.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReliabilityBucket {
    /// The midpoint of the prediction bucket.
    pub bucket_midpoint: f64,
    /// The number of predictions in this bucket.
    pub count: usize,
    /// The mean predicted value in this bucket.
    pub mean_predicted: f64,
    /// The mean observed value in this bucket.
    pub mean_observed: f64,
}

/// The number of buckets for the reliability diagram.
const RELIABILITY_BUCKETS: usize = 10;

/// Maximum number of raw records kept for the reliability diagram.
/// The online accumulators track all history for O(1) bias/slope/intercept,
/// but the reliability diagram needs raw records. This ring buffer caps
/// memory usage while still providing a recent reliability picture.
const MAX_RECORDS: usize = 500;

/// The calibration tracker — accumulates prediction-observation pairs and
/// produces calibration reports.
///
/// The brain records every prediction it makes and the corresponding
/// observation. Over time, this data is used to:
///
/// 1. **Detect bias**: if the brain consistently over-predicts, the bias
///    correction will discount future predictions.
/// 2. **Fix calibration**: if the calibration slope is < 1.0, the brain is
///    over-confident and should widen its credible intervals.
/// 3. **Self-criticize**: before making a decision, the brain applies the
///    calibration correction to its predictions, making the EFE score more
///    honest.
///
/// # Performance
///
/// The tracker maintains online accumulators (Σx, Σy, Σx², Σxy, Σerror,
/// Σabs_error, Σerror²) so that [`correct_prediction`] and [`correct_bias`]
/// are O(1). A bounded ring buffer of the most recent records is kept for
/// the reliability diagram in [`report`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CalibrationTracker {
    /// Bounded ring buffer of recent records for the reliability diagram.
    pub records: Vec<PredictionRecord>,
    /// Total number of predictions ever recorded (may exceed records.len()).
    pub total_count: usize,
    /// Online accumulators for O(1) calibration correction.
    sum_predicted: f64,
    sum_observed: f64,
    sum_predicted_sq: f64,
    sum_predicted_observed: f64,
    sum_error: f64,
    sum_abs_error: f64,
    sum_error_sq: f64,
}

impl CalibrationTracker {
    /// Creates a new, empty calibration tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a prediction-observation pair.
    pub fn record(&mut self, template_id: &str, predicted: f64, predicted_std: f64, observed: f64) {
        self.record_with_context(
            template_id,
            predicted,
            predicted_std,
            observed,
            None,
            None,
            None,
            None,
        );
    }

    /// Records a prediction-observation pair with full hierarchical context.
    /// The target, channel, estimation_regime, and evidence_quality enable
    /// calibration at progressively more specific levels:
    ///   global → template → channel → audience/target → context
    #[allow(clippy::too_many_arguments)]
    pub fn record_with_context(
        &mut self,
        template_id: &str,
        predicted: f64,
        predicted_std: f64,
        observed: f64,
        target: Option<&str>,
        channel: Option<&str>,
        estimation_regime: Option<&str>,
        evidence_quality: Option<&str>,
    ) {
        // Update online accumulators.
        let error = predicted - observed;
        self.total_count += 1;
        self.sum_predicted += predicted;
        self.sum_observed += observed;
        self.sum_predicted_sq += predicted * predicted;
        self.sum_predicted_observed += predicted * observed;
        self.sum_error += error;
        self.sum_abs_error += error.abs();
        self.sum_error_sq += error * error;

        // Maintain bounded ring buffer for reliability diagram.
        if self.records.len() >= MAX_RECORDS {
            self.records.remove(0);
        }
        self.records.push(PredictionRecord {
            predicted,
            predicted_std,
            observed,
            template_id: template_id.to_owned(),
            target: target.map(|s| s.to_owned()),
            channel: channel.map(|s| s.to_owned()),
            estimation_regime: estimation_regime.map(|s| s.to_owned()),
            evidence_quality: evidence_quality.map(|s| s.to_owned()),
        });
    }

    /// Returns the total number of recorded predictions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.total_count
    }

    /// Returns true if no predictions have been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total_count == 0
    }

    /// Produces a calibration report from the accumulated statistics.
    ///
    /// Computes bias, MAE, RMSE, calibration slope/intercept (via OLS
    /// regression of observed on predicted), and a reliability diagram
    /// with [`RELIABILITY_BUCKETS`] buckets from the recent record buffer.
    #[must_use]
    pub fn report(&self) -> CalibrationReport {
        let n = self.total_count;
        if n == 0 {
            return CalibrationReport::default();
        }
        let nf = n as f64;
        let bias = self.sum_error / nf;
        let mae = self.sum_abs_error / nf;
        let rmse = (self.sum_error_sq / nf).sqrt();

        // OLS regression from online accumulators:
        // slope = cov(x,y) / var(x), intercept = mean_y - slope * mean_x
        let mean_x = self.sum_predicted / nf;
        let mean_y = self.sum_observed / nf;
        let var_x = self.sum_predicted_sq / nf - mean_x * mean_x;
        let cov_xy = self.sum_predicted_observed / nf - mean_x * mean_y;
        let (slope, intercept) = if var_x < 1e-10 {
            (1.0, 0.0)
        } else {
            let s = cov_xy / var_x;
            (s, mean_y - s * mean_x)
        };

        // Reliability diagram from the bounded record buffer.
        let reliability = reliability_diagram(&self.records, RELIABILITY_BUCKETS);

        CalibrationReport {
            bias,
            mae,
            rmse,
            calibration_slope: slope,
            calibration_intercept: intercept,
            n,
            reliability,
        }
    }

    /// Applies calibration correction to a prediction.
    ///
    /// Uses the calibration slope and intercept to correct a new prediction:
    ///
    /// ```text
    /// corrected = slope * predicted + intercept
    /// ```
    ///
    /// If no data has been recorded, returns the prediction unchanged.
    /// This is O(1) — it uses online accumulators, not a full history scan.
    #[must_use]
    pub fn correct_prediction(&self, predicted: f64) -> f64 {
        if self.total_count == 0 {
            return predicted;
        }
        let nf = self.total_count as f64;
        let mean_x = self.sum_predicted / nf;
        let mean_y = self.sum_observed / nf;
        let var_x = self.sum_predicted_sq / nf - mean_x * mean_x;
        if var_x < 1e-10 {
            return predicted;
        }
        let cov_xy = self.sum_predicted_observed / nf - mean_x * mean_y;
        let slope = cov_xy / var_x;
        let intercept = mean_y - slope * mean_x;
        slope * predicted + intercept
    }

    /// Applies bias correction to a prediction.
    ///
    /// Simply subtracts the running bias from the prediction:
    ///
    /// ```text
    /// corrected = predicted - bias
    /// ```
    ///
    /// This is simpler than [`correct_prediction`] but more robust when the
    /// calibration slope is noisy (few observations). O(1) via online
    /// accumulators.
    #[must_use]
    pub fn correct_bias(&self, predicted: f64) -> f64 {
        if self.total_count == 0 {
            return predicted;
        }
        let bias = self.sum_error / self.total_count as f64;
        predicted - bias
    }

    /// Corrects prediction uncertainty based on calibration slope.
    ///
    /// CALIBRATION ≠ OBSERVABILITY — it must feed back into uncertainty.
    ///
    /// The calibration slope measures whether the brain's confidence
    /// scales correctly with prediction magnitude:
    /// - slope < 1.0 → over-confident → inflate uncertainty
    /// - slope > 1.0 → under-confident → deflate uncertainty
    /// - slope = 1.0 → well-calibrated → no change
    ///
    /// The correction: `corrected_std = predicted_std / max(slope, 0.1)`
    /// (clamped to prevent division by near-zero). This closes the loop:
    ///   prediction → outcome → calibration → uncertainty correction
    ///   → next prediction
    #[must_use]
    pub fn correct_uncertainty(&self, predicted_std: f64) -> f64 {
        if self.total_count < 5 {
            // Not enough data to trust the slope — return unchanged.
            return predicted_std;
        }
        let report = self.report();
        if report.calibration_slope < 1e-6 {
            // Slope is effectively zero — can't correct meaningfully.
            return predicted_std;
        }
        // Inflate when over-confident (slope < 1), deflate when under-confident.
        // Clamp slope to [0.1, 10.0] to prevent extreme corrections.
        let clamped_slope = report.calibration_slope.clamp(0.1, 10.0);
        predicted_std / clamped_slope
    }

    /// Returns the calibration bias for a specific template — the running
    /// mean of (predicted - observed) for that template. Returns 0.0 when
    /// no data exists for the template.
    #[must_use]
    pub fn bias_for_template(&self, template_id: &str) -> f64 {
        let filtered: Vec<&PredictionRecord> = self
            .records
            .iter()
            .filter(|r| r.template_id == template_id)
            .collect();
        if filtered.is_empty() {
            return 0.0;
        }
        let sum_error: f64 = filtered.iter().map(|r| r.predicted - r.observed).sum();
        sum_error / filtered.len() as f64
    }

    /// Returns the calibration report for a specific template.
    #[must_use]
    pub fn report_for_template(&self, template_id: &str) -> CalibrationReport {
        let filtered: Vec<&PredictionRecord> = self
            .records
            .iter()
            .filter(|r| r.template_id == template_id)
            .collect();
        if filtered.is_empty() {
            return CalibrationReport::default();
        }

        let n = filtered.len();
        let errors: Vec<f64> = filtered.iter().map(|r| r.predicted - r.observed).collect();

        let bias = errors.iter().sum::<f64>() / n as f64;
        let mae = errors.iter().map(|e| e.abs()).sum::<f64>() / n as f64;
        let rmse = (errors.iter().map(|e| e * e).sum::<f64>() / n as f64).sqrt();

        let records: Vec<PredictionRecord> = filtered.into_iter().cloned().collect();
        let (slope, intercept) = ols_regression(&records);
        let reliability = reliability_diagram(&records, RELIABILITY_BUCKETS);

        CalibrationReport {
            bias,
            mae,
            rmse,
            calibration_slope: slope,
            calibration_intercept: intercept,
            n,
            reliability,
        }
    }
}

/// Regime-isolated calibration — separate `CalibrationTracker` per
/// estimation regime.
///
/// CALIBRATION REGIME ISOLATION — different estimators have different
/// error-generating processes. A badly calibrated observational predictor
/// must NOT distort uncertainty for the randomized treatment estimator.
/// This struct ensures that Y30Direct, Y14Bridged, and OutcomeModel
/// calibration corrections are fully isolated.
///
/// The `EstimationRegime` is determined by the caller (the causal model
/// knows which regime it's using for each prediction). Each regime has
/// its own accumulator, slope, and intercept — they never bleed into
/// each other.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CalibrationByRegime {
    /// Y30 direct treatment-effect posterior — directly observed durable
    /// fans. Highest trust. No bridge uncertainty.
    pub y30_direct: CalibrationTracker,
    /// Y14 treatment effect + Y14→Y30 bridge model. The bridge inflates
    /// variance when uncalibrated. Medium trust.
    pub y14_bridged: CalibrationTracker,
    /// Outcome model only — no treatment-effect evidence. Observational.
    /// Lowest causal confidence.
    pub outcome_model: CalibrationTracker,
}

impl CalibrationByRegime {
    /// Creates a new, empty regime-isolated calibration tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a prediction-observation pair for the specified regime.
    /// Routes to the correct regime-specific tracker.
    ///
    /// The `evidence_quality` parameter should come from the actual
    /// measurement-determined quality (e.g. `ev.evidence_quality.as_str()`),
    /// NOT synthesized from the regime. This ensures that a Y30 measurement
    /// that fell back to workspace DiD is correctly recorded as
    /// `matched_quasi_experiment`, not `randomized_holdout`.
    #[allow(clippy::too_many_arguments)]
    pub fn record_by_regime(
        &mut self,
        regime: crate::decision_value::EstimationRegime,
        template_id: &str,
        predicted: f64,
        predicted_std: f64,
        observed: f64,
        target: Option<&str>,
        channel: Option<&str>,
        evidence_quality: &str,
    ) {
        match regime {
            crate::decision_value::EstimationRegime::Y30Direct => {
                self.y30_direct.record_with_context(
                    template_id,
                    predicted,
                    predicted_std,
                    observed,
                    target,
                    channel,
                    Some("y30_direct"),
                    Some(evidence_quality),
                );
            }
            crate::decision_value::EstimationRegime::Y14Bridged => {
                self.y14_bridged.record_with_context(
                    template_id,
                    predicted,
                    predicted_std,
                    observed,
                    target,
                    channel,
                    Some("y14_bridged"),
                    Some(evidence_quality),
                );
            }
            crate::decision_value::EstimationRegime::OutcomeModel => {
                self.outcome_model.record_with_context(
                    template_id,
                    predicted,
                    predicted_std,
                    observed,
                    target,
                    channel,
                    Some("outcome_model"),
                    Some(evidence_quality),
                );
            }
        }
    }

    /// Corrects a prediction using the regime-specific calibration.
    /// Uses the slope and intercept from the correct regime tracker.
    #[must_use]
    pub fn correct_prediction_by_regime(
        &self,
        regime: crate::decision_value::EstimationRegime,
        predicted: f64,
    ) -> f64 {
        match regime {
            crate::decision_value::EstimationRegime::Y30Direct => {
                self.y30_direct.correct_prediction(predicted)
            }
            crate::decision_value::EstimationRegime::Y14Bridged => {
                self.y14_bridged.correct_prediction(predicted)
            }
            crate::decision_value::EstimationRegime::OutcomeModel => {
                self.outcome_model.correct_prediction(predicted)
            }
        }
    }

    /// Corrects prediction uncertainty using the regime-specific
    /// calibration slope. This is the key method for regime isolation:
    /// a bad OutcomeModel calibration slope must NOT inflate Y30Direct
    /// uncertainty.
    #[must_use]
    pub fn correct_uncertainty_by_regime(
        &self,
        regime: crate::decision_value::EstimationRegime,
        predicted_std: f64,
    ) -> f64 {
        match regime {
            crate::decision_value::EstimationRegime::Y30Direct => {
                self.y30_direct.correct_uncertainty(predicted_std)
            }
            crate::decision_value::EstimationRegime::Y14Bridged => {
                self.y14_bridged.correct_uncertainty(predicted_std)
            }
            crate::decision_value::EstimationRegime::OutcomeModel => {
                self.outcome_model.correct_uncertainty(predicted_std)
            }
        }
    }

    /// Returns the calibration bias for a specific template from the
    /// regime-specific tracker.
    #[must_use]
    pub fn bias_for_template_by_regime(
        &self,
        regime: crate::decision_value::EstimationRegime,
        template_id: &str,
    ) -> f64 {
        match regime {
            crate::decision_value::EstimationRegime::Y30Direct => {
                self.y30_direct.bias_for_template(template_id)
            }
            crate::decision_value::EstimationRegime::Y14Bridged => {
                self.y14_bridged.bias_for_template(template_id)
            }
            crate::decision_value::EstimationRegime::OutcomeModel => {
                self.outcome_model.bias_for_template(template_id)
            }
        }
    }
}

/// Ordinary Least Squares regression: observed = slope * predicted + intercept.
///
/// Returns `(slope, intercept)`. If the variance of predictions is zero
/// (all predictions are the same), returns `(1.0, 0.0)` (no correction).
fn ols_regression(records: &[PredictionRecord]) -> (f64, f64) {
    if records.is_empty() {
        return (1.0, 0.0);
    }
    let n = records.len() as f64;
    let mean_x: f64 = records.iter().map(|r| r.predicted).sum::<f64>() / n;
    let mean_y: f64 = records.iter().map(|r| r.observed).sum::<f64>() / n;

    let var_x: f64 = records
        .iter()
        .map(|r| (r.predicted - mean_x).powi(2))
        .sum::<f64>()
        / n;

    if var_x < 1e-10 {
        // All predictions are the same — can't fit a slope.
        return (1.0, 0.0);
    }

    let cov_xy: f64 = records
        .iter()
        .map(|r| (r.predicted - mean_x) * (r.observed - mean_y))
        .sum::<f64>()
        / n;

    let slope = cov_xy / var_x;
    let intercept = mean_y - slope * mean_x;
    (slope, intercept)
}

/// Builds a reliability diagram by bucketing predictions and comparing
/// the mean predicted value to the mean observed value in each bucket.
fn reliability_diagram(records: &[PredictionRecord], n_buckets: usize) -> Vec<ReliabilityBucket> {
    if records.is_empty() {
        return Vec::new();
    }

    let mut sorted: Vec<&PredictionRecord> = records.iter().collect();
    sorted.sort_by(|a, b| {
        a.predicted
            .partial_cmp(&b.predicted)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let bucket_size = sorted.len().div_ceil(n_buckets).max(1);
    let mut buckets = Vec::new();

    for chunk in sorted.chunks(bucket_size) {
        if chunk.is_empty() {
            continue;
        }
        let count = chunk.len();
        let mean_predicted: f64 = chunk.iter().map(|r| r.predicted).sum::<f64>() / count as f64;
        let mean_observed: f64 = chunk.iter().map(|r| r.observed).sum::<f64>() / count as f64;
        buckets.push(ReliabilityBucket {
            bucket_midpoint: mean_predicted,
            count,
            mean_predicted,
            mean_observed,
        });
    }

    buckets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tracker_has_empty_report() {
        let tracker = CalibrationTracker::new();
        assert!(tracker.is_empty());
        let report = tracker.report();
        assert_eq!(report.n, 0);
        assert!((report.bias).abs() < 0.001);
        assert!((report.mae).abs() < 0.001);
    }

    #[test]
    fn record_adds_entry() {
        let mut tracker = CalibrationTracker::new();
        tracker.record("reddit-scanner", 10.0, 2.0, 8.0);
        assert_eq!(tracker.len(), 1);
        assert!(!tracker.is_empty());
    }

    #[test]
    fn bias_detects_over_prediction() {
        let mut tracker = CalibrationTracker::new();
        // All predictions are 10, all observations are 7 → bias = 3.
        for _ in 0..10 {
            tracker.record("t", 10.0, 2.0, 7.0);
        }
        let report = tracker.report();
        assert!(
            (report.bias - 3.0).abs() < 0.001,
            "bias should be 3.0, got {}",
            report.bias
        );
    }

    #[test]
    fn bias_detects_under_prediction() {
        let mut tracker = CalibrationTracker::new();
        for _ in 0..10 {
            tracker.record("t", 5.0, 2.0, 8.0);
        }
        let report = tracker.report();
        assert!(
            (report.bias - (-3.0)).abs() < 0.001,
            "bias should be -3.0, got {}",
            report.bias
        );
    }

    #[test]
    fn mae_computes_mean_absolute_error() {
        let mut tracker = CalibrationTracker::new();
        tracker.record("t", 10.0, 1.0, 7.0); // error = 3
        tracker.record("t", 5.0, 1.0, 8.0); // error = -3
        let report = tracker.report();
        assert!(
            (report.mae - 3.0).abs() < 0.001,
            "MAE should be 3.0, got {}",
            report.mae
        );
    }

    #[test]
    fn rmse_computes_root_mean_squared_error() {
        let mut tracker = CalibrationTracker::new();
        tracker.record("t", 10.0, 1.0, 7.0); // error = 3, sq = 9
        tracker.record("t", 5.0, 1.0, 8.0); // error = -3, sq = 9
        let report = tracker.report();
        assert!(
            (report.rmse - 3.0).abs() < 0.001,
            "RMSE should be 3.0, got {}",
            report.rmse
        );
    }

    #[test]
    fn calibration_slope_perfect_predictions() {
        let mut tracker = CalibrationTracker::new();
        // observed = predicted → slope = 1.0, intercept = 0.0
        for i in 0..10 {
            let v = i as f64 * 2.0;
            tracker.record("t", v, 1.0, v);
        }
        let report = tracker.report();
        assert!(
            (report.calibration_slope - 1.0).abs() < 0.01,
            "perfect calibration → slope=1.0, got {}",
            report.calibration_slope
        );
        assert!(
            report.calibration_intercept.abs() < 0.01,
            "perfect calibration → intercept=0.0, got {}",
            report.calibration_intercept
        );
    }

    #[test]
    fn calibration_slope_over_confident() {
        let mut tracker = CalibrationTracker::new();
        // observed = 0.5 * predicted → slope = 0.5 (over-confident)
        for i in 0..10 {
            let v = i as f64 * 2.0 + 1.0;
            tracker.record("t", v, 1.0, 0.5 * v);
        }
        let report = tracker.report();
        assert!(
            (report.calibration_slope - 0.5).abs() < 0.01,
            "over-confident → slope=0.5, got {}",
            report.calibration_slope
        );
    }

    #[test]
    fn correct_prediction_applies_calibration() {
        let mut tracker = CalibrationTracker::new();
        // observed = 0.5 * predicted + 1.0
        for i in 0..10 {
            let v = i as f64 * 2.0 + 1.0;
            tracker.record("t", v, 1.0, 0.5 * v + 1.0);
        }
        // Correct a new prediction of 20.0.
        // corrected = 0.5 * 20.0 + 1.0 = 11.0
        let corrected = tracker.correct_prediction(20.0);
        assert!(
            (corrected - 11.0).abs() < 0.5,
            "corrected prediction should be ~11.0, got {corrected}"
        );
    }

    #[test]
    fn correct_prediction_returns_unchanged_when_empty() {
        let tracker = CalibrationTracker::new();
        assert!((tracker.correct_prediction(10.0) - 10.0).abs() < 0.001);
    }

    #[test]
    fn correct_bias_subtracts_bias() {
        let mut tracker = CalibrationTracker::new();
        // bias = 3.0 (over-predicting by 3)
        for _ in 0..10 {
            tracker.record("t", 10.0, 1.0, 7.0);
        }
        // correct_bias(10.0) = 10.0 - 3.0 = 7.0
        let corrected = tracker.correct_bias(10.0);
        assert!(
            (corrected - 7.0).abs() < 0.001,
            "bias-corrected should be 7.0, got {corrected}"
        );
    }

    #[test]
    fn correct_bias_returns_unchanged_when_empty() {
        let tracker = CalibrationTracker::new();
        assert!((tracker.correct_bias(10.0) - 10.0).abs() < 0.001);
    }

    #[test]
    fn reliability_diagram_has_buckets() {
        let mut tracker = CalibrationTracker::new();
        for i in 0..20 {
            let v = i as f64;
            tracker.record("t", v, 1.0, v);
        }
        let report = tracker.report();
        assert!(!report.reliability.is_empty());
        // With 20 records and 10 buckets, each bucket has 2 records.
        for bucket in &report.reliability {
            assert_eq!(bucket.count, 2);
            // Perfect calibration: mean_predicted == mean_observed.
            assert!((bucket.mean_predicted - bucket.mean_observed).abs() < 0.001);
        }
    }

    #[test]
    fn report_for_template_filters_correctly() {
        let mut tracker = CalibrationTracker::new();
        tracker.record("a", 10.0, 1.0, 8.0);
        tracker.record("b", 10.0, 1.0, 5.0);
        tracker.record("a", 10.0, 1.0, 8.0);

        let report_a = tracker.report_for_template("a");
        assert_eq!(report_a.n, 2);
        assert!(
            (report_a.bias - 2.0).abs() < 0.001,
            "template a bias should be 2.0"
        );

        let report_b = tracker.report_for_template("b");
        assert_eq!(report_b.n, 1);
        assert!(
            (report_b.bias - 5.0).abs() < 0.001,
            "template b bias should be 5.0"
        );
    }

    #[test]
    fn report_for_unknown_template_is_empty() {
        let mut tracker = CalibrationTracker::new();
        tracker.record("a", 10.0, 1.0, 8.0);
        let report = tracker.report_for_template("unknown");
        assert_eq!(report.n, 0);
    }

    #[test]
    fn calibration_tracker_serializes() {
        let mut tracker = CalibrationTracker::new();
        tracker.record("t", 10.0, 2.0, 8.0);
        let json = serde_json::to_string(&tracker).expect("should serialize");
        assert!(json.contains("records"));
        assert!(json.contains("template_id"));
    }

    #[test]
    fn calibration_report_serializes() {
        let mut tracker = CalibrationTracker::new();
        tracker.record("t", 10.0, 2.0, 8.0);
        let report = tracker.report();
        let json = serde_json::to_string(&report).expect("should serialize");
        assert!(json.contains("bias"));
        assert!(json.contains("rmse"));
        assert!(json.contains("calibration_slope"));
    }

    #[test]
    fn ols_regression_constant_predictions_returns_identity() {
        let records = vec![
            PredictionRecord {
                predicted: 5.0,
                predicted_std: 1.0,
                observed: 3.0,
                template_id: "t".to_owned(),
                target: None,
                channel: None,
                estimation_regime: None,
                evidence_quality: None,
            },
            PredictionRecord {
                predicted: 5.0,
                predicted_std: 1.0,
                observed: 7.0,
                template_id: "t".to_owned(),
                target: None,
                channel: None,
                estimation_regime: None,
                evidence_quality: None,
            },
        ];
        let (slope, intercept) = ols_regression(&records);
        assert!((slope - 1.0).abs() < 0.001);
        assert!((intercept).abs() < 0.001);
    }
}
