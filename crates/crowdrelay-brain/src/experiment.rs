//! Experiment engine — controlled experiments with propensity logging.
//!
//! The brain doesn't just dispatch and hope — it runs controlled experiments
//! to measure causal uplift. When the brain is uncertain about a template's
//! effectiveness, it randomly assigns some dispatches to treatment (dispatch)
//! and some to control (don't dispatch), then compares the outcomes.
//!
//! # Propensity logging
//!
//! The key to off-policy evaluation is **propensity logging**: recording the
//! probability that the brain assigned each dispatch to treatment or control.
//! Without this, the brain can't correct for selection bias when analyzing
//! historical data.
//!
//! For each dispatch, the brain records:
//! - `selection_probability`: the probability that this opportunity was
//!   selected for dispatch (from the softmax or greedy policy)
//! - `treatment_assignment`: whether this dispatch was treatment or control
//! - `assignment_probability`: the probability of the treatment assignment
//!
//! # Experiment design
//!
//! The brain uses a simple but effective design:
//! - When a template has low confidence (< MIN_CONFIDENCE_FOR_EXPERIMENT),
//!   the brain runs a 50/50 experiment: 50% treatment, 50% control.
//! - When confidence is high, the brain dispatches greedily (no experiment).
//! - The experiment continues until the template reaches sufficient confidence
//!   or the effect is clearly positive/negative.
//!
//! # Off-policy correction
//!
//! When analyzing historical data, the brain uses inverse propensity weighting
//! (IPW) to correct for selection bias:
//!
//! ```text
//! IPW_estimate = Σ (treatment × outcome / propensity) / Σ (treatment / propensity)
//! ```

use serde::Serialize;

/// Minimum confidence (observation count) before the brain stops experimenting
/// and dispatches greedily. Below this, the brain runs 50/50 experiments.
pub const MIN_CONFIDENCE_FOR_EXPERIMENT: u32 = 10;

/// The default treatment probability during experiments (50/50).
pub const DEFAULT_TREATMENT_PROBABILITY: f64 = 0.5;

/// The treatment assignment for a dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TreatmentAssignment {
    /// The dispatch was performed (treatment group).
    Treatment,
    /// The dispatch was withheld (control group).
    Control,
}

impl TreatmentAssignment {
    /// Returns 1.0 for treatment, 0.0 for control — used in IPW calculations.
    #[must_use]
    pub const fn indicator(self) -> f64 {
        match self {
            Self::Treatment => 1.0,
            Self::Control => 0.0,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Treatment => "treatment",
            Self::Control => "control",
        }
    }
}

/// A propensity record — logged for every dispatch decision the brain makes.
/// This is the data that enables off-policy evaluation.
#[derive(Clone, Debug, Serialize)]
pub struct PropensityRecord {
    /// The opportunity ID (stable across cycles).
    pub opportunity_key: String,
    /// The worker template that was considered.
    pub template_id: String,
    /// The probability that this opportunity was selected for dispatch
    /// (from the softmax or greedy policy).
    pub selection_probability: f64,
    /// Whether this dispatch was treatment or control.
    pub treatment: TreatmentAssignment,
    /// The probability of the treatment assignment (e.g. 0.5 for 50/50).
    pub assignment_probability: f64,
    /// The EFE score at the time of decision.
    pub efe_score: f64,
    /// The brain's policy version (for tracking changes over time).
    pub policy_version: u32,
}

/// The experiment engine — decides when to experiment and logs propensities.
///
/// `Default` returns the same configuration as [`ExperimentEngine::new`]:
/// `policy_version = 1`, `treatment_probability = 0.5`,
/// `min_confidence = 10`. A derived `Default` would yield
/// `treatment_probability = 0.0`, which never runs an experiment, so it is
/// implemented explicitly.
#[derive(Clone, Debug, Serialize)]
pub struct ExperimentEngine {
    /// The brain's policy version. Incremented when the policy changes.
    pub policy_version: u32,
    /// The treatment probability for experiments (default 0.5).
    pub treatment_probability: f64,
    /// Minimum confidence before stopping experiments.
    pub min_confidence: u32,
}

impl Default for ExperimentEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ExperimentEngine {
    /// Creates a new experiment engine with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            policy_version: 1,
            treatment_probability: DEFAULT_TREATMENT_PROBABILITY,
            min_confidence: MIN_CONFIDENCE_FOR_EXPERIMENT,
        }
    }

    /// Decides whether to run an experiment for a template given its confidence.
    ///
    /// Returns `Some(TreatmentAssignment)` if the brain should make an
    /// experiment decision, or `None` if the brain should dispatch greedily
    /// (high confidence — no experiment needed).
    ///
    /// The `random_draw` parameter is a uniform random number in [0, 1).
    /// This is passed in (not generated internally) so the brain remains
    /// deterministic and testable.
    #[must_use]
    pub fn assign_treatment(
        &self,
        confidence: u32,
        random_draw: f64,
    ) -> Option<TreatmentAssignment> {
        if confidence >= self.min_confidence {
            // High confidence: dispatch greedily, no experiment.
            return None;
        }
        // Low confidence: run experiment.
        let treatment = if random_draw < self.treatment_probability {
            TreatmentAssignment::Treatment
        } else {
            TreatmentAssignment::Control
        };
        Some(treatment)
    }

    /// Creates a propensity record for a dispatch decision.
    #[must_use]
    pub fn record_propensity(
        &self,
        opportunity_key: String,
        template_id: String,
        selection_probability: f64,
        treatment: TreatmentAssignment,
        efe_score: f64,
    ) -> PropensityRecord {
        PropensityRecord {
            opportunity_key,
            template_id,
            selection_probability,
            treatment,
            assignment_probability: self.treatment_probability,
            efe_score,
            policy_version: self.policy_version,
        }
    }

    /// Computes the inverse propensity weight for a propensity record.
    ///
    /// IPW = treatment_indicator / assignment_probability
    ///
    /// This weight is used to correct for selection bias when estimating
    /// the average treatment effect from historical data.
    #[must_use]
    pub fn ipw_weight(record: &PropensityRecord) -> f64 {
        if record.assignment_probability <= 0.0 || record.assignment_probability > 1.0 {
            return 0.0;
        }
        record.treatment.indicator() / record.assignment_probability
    }

    /// Estimates the average treatment effect (ATE) from a set of propensity
    /// records and their observed outcomes.
    ///
    /// Uses the IPW estimator:
    /// ```text
    /// ATE = Σ(treatment × outcome / propensity) / Σ(treatment / propensity)
    ///     - Σ(control × outcome / (1-propensity)) / Σ(control / (1-propensity))
    /// ```
    ///
    /// Returns `None` if there's insufficient data (no treatment or control
    /// observations).
    #[must_use]
    pub fn estimate_ate(records: &[PropensityRecord], outcomes: &[f64]) -> Option<f64> {
        if records.len() != outcomes.len() || records.is_empty() {
            return None;
        }
        let mut treatment_weighted_outcome = 0.0;
        let mut treatment_weight_sum = 0.0;
        let mut control_weighted_outcome = 0.0;
        let mut control_weight_sum = 0.0;
        for (record, &outcome) in records.iter().zip(outcomes.iter()) {
            let p = record.assignment_probability;
            if p <= 0.0 || p >= 1.0 {
                continue;
            }
            match record.treatment {
                TreatmentAssignment::Treatment => {
                    let weight = 1.0 / p;
                    treatment_weighted_outcome += weight * outcome;
                    treatment_weight_sum += weight;
                }
                TreatmentAssignment::Control => {
                    let weight = 1.0 / (1.0 - p);
                    control_weighted_outcome += weight * outcome;
                    control_weight_sum += weight;
                }
            }
        }
        if treatment_weight_sum <= 0.0 || control_weight_sum <= 0.0 {
            return None;
        }
        let treatment_mean = treatment_weighted_outcome / treatment_weight_sum;
        let control_mean = control_weighted_outcome / control_weight_sum;
        Some(treatment_mean - control_mean)
    }

    /// Estimates the paired treatment effect and its variance from a set of
    /// propensity records and observed outcomes.
    ///
    /// Returns `(tau, variance)` where `tau` is the IPW-estimated treatment
    /// effect and `variance` is the variance of the estimate. The variance
    /// is computed using the IPW estimator's asymptotic variance:
    ///
    /// ```text
    /// Var(τ) = Σ w_i² (y_i - μ_group)² / (Σ w_i)²
    /// ```
    ///
    /// where `w_i` is the IPW weight and `μ_group` is the weighted group mean.
    /// This is a simplified variance estimator that ignores the covariance
    /// between treatment and control groups (conservative).
    ///
    /// Returns `None` if there's insufficient data.
    #[must_use]
    pub fn estimate_paired_treatment_effect(
        records: &[PropensityRecord],
        outcomes: &[f64],
    ) -> Option<(f64, f64)> {
        if records.len() != outcomes.len() || records.is_empty() {
            return None;
        }
        let mut treatment_weighted_outcome = 0.0;
        let mut treatment_weight_sum = 0.0;
        let mut control_weighted_outcome = 0.0;
        let mut control_weight_sum = 0.0;
        // Collect (weight, outcome) pairs for variance computation.
        let mut treatment_pairs: Vec<(f64, f64)> = Vec::new();
        let mut control_pairs: Vec<(f64, f64)> = Vec::new();
        for (record, &outcome) in records.iter().zip(outcomes.iter()) {
            let p = record.assignment_probability;
            if p <= 0.0 || p >= 1.0 {
                continue;
            }
            match record.treatment {
                TreatmentAssignment::Treatment => {
                    let weight = 1.0 / p;
                    treatment_weighted_outcome += weight * outcome;
                    treatment_weight_sum += weight;
                    treatment_pairs.push((weight, outcome));
                }
                TreatmentAssignment::Control => {
                    let weight = 1.0 / (1.0 - p);
                    control_weighted_outcome += weight * outcome;
                    control_weight_sum += weight;
                    control_pairs.push((weight, outcome));
                }
            }
        }
        if treatment_weight_sum <= 0.0 || control_weight_sum <= 0.0 {
            return None;
        }
        let treatment_mean = treatment_weighted_outcome / treatment_weight_sum;
        let control_mean = control_weighted_outcome / control_weight_sum;
        let tau = treatment_mean - control_mean;
        // Variance: Σ w_i² (y_i - μ)² / (Σ w_i)² for each group, then sum.
        let treatment_var: f64 = treatment_pairs
            .iter()
            .map(|(w, y)| w * w * (y - treatment_mean).powi(2))
            .sum::<f64>()
            / (treatment_weight_sum * treatment_weight_sum);
        let control_var: f64 = control_pairs
            .iter()
            .map(|(w, y)| w * w * (y - control_mean).powi(2))
            .sum::<f64>()
            / (control_weight_sum * control_weight_sum);
        let variance = treatment_var + control_var;
        Some((tau, variance))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_confidence_triggers_experiment() {
        let engine = ExperimentEngine::new();
        let assignment = engine.assign_treatment(5, 0.3);
        assert!(assignment.is_some());
    }

    #[test]
    fn high_confidence_dispatches_greedily() {
        let engine = ExperimentEngine::new();
        let assignment = engine.assign_treatment(20, 0.3);
        assert!(assignment.is_none());
    }

    #[test]
    fn treatment_assignment_uses_random_draw() {
        let engine = ExperimentEngine::new();
        // random_draw < 0.5 → treatment
        assert_eq!(
            engine.assign_treatment(0, 0.3),
            Some(TreatmentAssignment::Treatment)
        );
        // random_draw >= 0.5 → control
        assert_eq!(
            engine.assign_treatment(0, 0.7),
            Some(TreatmentAssignment::Control)
        );
    }

    #[test]
    fn treatment_indicator() {
        assert_eq!(TreatmentAssignment::Treatment.indicator(), 1.0);
        assert_eq!(TreatmentAssignment::Control.indicator(), 0.0);
    }

    #[test]
    fn propensity_record_contains_all_fields() {
        let engine = ExperimentEngine::new();
        let record = engine.record_propensity(
            "community-engager:r_MetalMusic:post:1:Std".to_owned(),
            "community-engager".to_owned(),
            0.8,
            TreatmentAssignment::Treatment,
            -5.0,
        );
        assert_eq!(record.template_id, "community-engager");
        assert_eq!(record.selection_probability, 0.8);
        assert_eq!(record.treatment, TreatmentAssignment::Treatment);
        assert_eq!(record.assignment_probability, 0.5);
        assert_eq!(record.efe_score, -5.0);
        assert_eq!(record.policy_version, 1);
    }

    #[test]
    fn ipw_weight_for_treatment() {
        let engine = ExperimentEngine::new();
        let record = engine.record_propensity(
            "key".to_owned(),
            "t".to_owned(),
            1.0,
            TreatmentAssignment::Treatment,
            -5.0,
        );
        // IPW = 1.0 / 0.5 = 2.0
        assert!((ExperimentEngine::ipw_weight(&record) - 2.0).abs() < 0.01);
    }

    #[test]
    fn ipw_weight_for_control_is_zero() {
        let engine = ExperimentEngine::new();
        let record = engine.record_propensity(
            "key".to_owned(),
            "t".to_owned(),
            1.0,
            TreatmentAssignment::Control,
            -5.0,
        );
        // IPW = 0.0 / 0.5 = 0.0 (control indicator is 0)
        assert!((ExperimentEngine::ipw_weight(&record) - 0.0).abs() < 0.01);
    }

    #[test]
    fn estimate_ate_with_clear_treatment_effect() {
        let engine = ExperimentEngine::new();
        // Treatment group: all outcomes = 10 (the action worked)
        // Control group: all outcomes = 2 (baseline)
        // ATE should be 10 - 2 = 8
        let records: Vec<PropensityRecord> = (0..10)
            .map(|i| {
                engine.record_propensity(
                    format!("key_{i}"),
                    "t".to_owned(),
                    1.0,
                    if i < 5 {
                        TreatmentAssignment::Treatment
                    } else {
                        TreatmentAssignment::Control
                    },
                    -5.0,
                )
            })
            .collect();
        let outcomes: Vec<f64> = (0..10).map(|i| if i < 5 { 10.0 } else { 2.0 }).collect();
        let ate = ExperimentEngine::estimate_ate(&records, &outcomes);
        assert!(ate.is_some());
        assert!((ate.unwrap() - 8.0).abs() < 0.01);
    }

    #[test]
    fn estimate_ate_returns_none_with_only_treatment() {
        let engine = ExperimentEngine::new();
        let records: Vec<PropensityRecord> = (0..5)
            .map(|_| {
                engine.record_propensity(
                    "key".to_owned(),
                    "t".to_owned(),
                    1.0,
                    TreatmentAssignment::Treatment,
                    -5.0,
                )
            })
            .collect();
        let outcomes = vec![10.0; 5];
        assert!(ExperimentEngine::estimate_ate(&records, &outcomes).is_none());
    }

    #[test]
    fn estimate_ate_returns_none_with_mismatched_lengths() {
        let engine = ExperimentEngine::new();
        let records = vec![engine.record_propensity(
            "key".to_owned(),
            "t".to_owned(),
            1.0,
            TreatmentAssignment::Treatment,
            -5.0,
        )];
        let outcomes = vec![10.0, 5.0];
        assert!(ExperimentEngine::estimate_ate(&records, &outcomes).is_none());
    }

    #[test]
    fn estimate_ate_returns_none_with_empty_data() {
        let ate = ExperimentEngine::estimate_ate(&[], &[]);
        assert!(ate.is_none());
    }

    #[test]
    fn estimate_ate_with_zero_effect() {
        let engine = ExperimentEngine::new();
        // Treatment and control have same outcomes → ATE ≈ 0
        let records: Vec<PropensityRecord> = (0..10)
            .map(|i| {
                engine.record_propensity(
                    format!("key_{i}"),
                    "t".to_owned(),
                    1.0,
                    if i < 5 {
                        TreatmentAssignment::Treatment
                    } else {
                        TreatmentAssignment::Control
                    },
                    -5.0,
                )
            })
            .collect();
        let outcomes = vec![5.0; 10];
        let ate = ExperimentEngine::estimate_ate(&records, &outcomes);
        assert!(ate.is_some());
        assert!(ate.unwrap().abs() < 0.01);
    }

    #[test]
    fn estimate_ate_with_negative_effect() {
        let engine = ExperimentEngine::new();
        // Treatment: outcomes = 1 (action hurt)
        // Control: outcomes = 5 (baseline was better)
        // ATE = 1 - 5 = -4
        let records: Vec<PropensityRecord> = (0..10)
            .map(|i| {
                engine.record_propensity(
                    format!("key_{i}"),
                    "t".to_owned(),
                    1.0,
                    if i < 5 {
                        TreatmentAssignment::Treatment
                    } else {
                        TreatmentAssignment::Control
                    },
                    -5.0,
                )
            })
            .collect();
        let outcomes: Vec<f64> = (0..10).map(|i| if i < 5 { 1.0 } else { 5.0 }).collect();
        let ate = ExperimentEngine::estimate_ate(&records, &outcomes);
        assert!(ate.is_some());
        assert!((ate.unwrap() - (-4.0)).abs() < 0.01);
    }

    #[test]
    fn treatment_assignment_as_str() {
        assert_eq!(TreatmentAssignment::Treatment.as_str(), "treatment");
        assert_eq!(TreatmentAssignment::Control.as_str(), "control");
    }

    #[test]
    fn experiment_engine_default_has_sensible_values() {
        let engine = ExperimentEngine::new();
        assert_eq!(engine.treatment_probability, 0.5);
        assert_eq!(engine.min_confidence, 10);
        assert_eq!(engine.policy_version, 1);
    }

    #[test]
    fn ipw_weight_zero_for_invalid_probability() {
        let engine = ExperimentEngine::new();
        let mut record = engine.record_propensity(
            "key".to_owned(),
            "t".to_owned(),
            1.0,
            TreatmentAssignment::Treatment,
            -5.0,
        );
        record.assignment_probability = 0.0;
        assert_eq!(ExperimentEngine::ipw_weight(&record), 0.0);
        record.assignment_probability = 1.5;
        assert_eq!(ExperimentEngine::ipw_weight(&record), 0.0);
    }

    #[test]
    fn estimate_paired_treatment_effect_matches_ate() {
        let engine = ExperimentEngine::new();
        let records: Vec<PropensityRecord> = (0..10)
            .map(|i| {
                engine.record_propensity(
                    format!("key_{i}"),
                    "t".to_owned(),
                    1.0,
                    if i < 5 {
                        TreatmentAssignment::Treatment
                    } else {
                        TreatmentAssignment::Control
                    },
                    -5.0,
                )
            })
            .collect();
        let outcomes: Vec<f64> = (0..10).map(|i| if i < 5 { 10.0 } else { 5.0 }).collect();
        let ate = ExperimentEngine::estimate_ate(&records, &outcomes).unwrap();
        let (tau, _) = ExperimentEngine::estimate_paired_treatment_effect(&records, &outcomes)
            .expect("should return Some");
        // tau should match ATE.
        assert!(
            (tau - ate).abs() < 0.01,
            "paired tau should match ATE, got tau={tau}, ate={ate}"
        );
    }

    #[test]
    fn estimate_paired_treatment_effect_returns_variance() {
        let engine = ExperimentEngine::new();
        let records: Vec<PropensityRecord> = (0..10)
            .map(|i| {
                engine.record_propensity(
                    format!("key_{i}"),
                    "t".to_owned(),
                    1.0,
                    if i < 5 {
                        TreatmentAssignment::Treatment
                    } else {
                        TreatmentAssignment::Control
                    },
                    -5.0,
                )
            })
            .collect();
        let outcomes: Vec<f64> = (0..10)
            .map(|i| {
                if i < 5 {
                    10.0 + (i as f64) * 2.0 // treatment: 10, 12, 14, 16, 18
                } else {
                    5.0 + ((i - 5) as f64) * 1.5 // control: 5, 6.5, 8, 9.5, 11
                }
            })
            .collect();
        let (_, variance) = ExperimentEngine::estimate_paired_treatment_effect(&records, &outcomes)
            .expect("should return Some");
        assert!(
            variance > 0.0,
            "variance should be positive, got {variance}"
        );
    }

    #[test]
    fn estimate_paired_treatment_effect_returns_none_for_insufficient_data() {
        let engine = ExperimentEngine::new();
        // Only treatment, no control.
        let records: Vec<PropensityRecord> = (0..5)
            .map(|i| {
                engine.record_propensity(
                    format!("key_{i}"),
                    "t".to_owned(),
                    1.0,
                    TreatmentAssignment::Treatment,
                    -5.0,
                )
            })
            .collect();
        let outcomes: Vec<f64> = vec![10.0; 5];
        assert!(ExperimentEngine::estimate_paired_treatment_effect(&records, &outcomes).is_none());
    }

    #[test]
    fn estimate_paired_treatment_effect_negative_tau() {
        let engine = ExperimentEngine::new();
        let records: Vec<PropensityRecord> = (0..10)
            .map(|i| {
                engine.record_propensity(
                    format!("key_{i}"),
                    "t".to_owned(),
                    1.0,
                    if i < 5 {
                        TreatmentAssignment::Treatment
                    } else {
                        TreatmentAssignment::Control
                    },
                    -5.0,
                )
            })
            .collect();
        // Treatment outcomes worse than control → negative tau.
        let outcomes: Vec<f64> = (0..10).map(|i| if i < 5 { 1.0 } else { 5.0 }).collect();
        let (tau, _) = ExperimentEngine::estimate_paired_treatment_effect(&records, &outcomes)
            .expect("should return Some");
        assert!(tau < 0.0, "negative effect expected, got {tau}");
    }
}
