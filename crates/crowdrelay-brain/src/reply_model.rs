//! Reply probability model — P(positive reply | kind, target, features).
//!
//! The outreach system ranks eligible targets by a single hand-tuned
//! `relevance_basis_points` field. When wave capacity is smaller than the
//! eligible-target pool, the system cannot learn that some targets are more
//! likely to reply than others — it just takes them in relevance order.
//!
//! This module adds a hierarchical Beta-Bernoulli model that predicts the
//! probability of a positive reply for each eligible target. The prediction
//! is a **bounded advisory signal**: it reorders eligible targets, it does
//! not change eligibility, authority, or approval.
//!
//! # North Star
//!
//! Better P(reply) ranking → higher positive reply rate per wave → more
//! playlist placements → more streams → more fan discovery → more
//! incremental durable fans. This is a direct fan-growth funnel lever.
//!
//! # Additive and reversible
//!
//! This model is a prediction signal only. It does NOT modify any existing
//! brain posterior, causal effect, EFE score, policy disposition, or
//! portfolio constraint. Deleting the model (empty state) returns the system
//! to `relevance_basis_points` ranking — the brain behaves exactly as before.
//!
//! # Hierarchy
//!
//! The model pools across three levels:
//!
//! 1. **Global** — the base reply rate across all outreach.
//! 2. **By kind** — per-target-kind (playlist, radio, press, ...).
//! 3. **By target** — per-specific-target (one curator, one station, ...).
//!
//! When a target has few observations, its prediction shrinks toward the
//! kind-level posterior. When a kind has few observations, it shrinks toward
//! the global posterior. This is the same partial-pooling structure the
//! existing [`HierarchicalPosterior`] and [`HierarchicalNegBinPosterior`]
//! use for Normal and NegBin models.
//!
//! # Feature extraction
//!
//! The model uses features already available in the outreach snapshot. All
//! features are known at dispatch time — no future-derived information, no
//! leakage. The feature set is versioned so a change in extraction logic
//! produces a new feature version, and old predictions remain auditable.
//!
//! # Fail-closed behavior
//!
//! No data → return global prior → relevance ranking is the fallback. The
//! model never crashes, never blocks a dispatch, and never overrides a
//! policy gate.

use serde::{Deserialize, Serialize};

use crate::beta::{BetaPosterior, HierarchicalBetaPosterior};

/// Current feature extraction version. Increment when the feature set
/// changes, so old predictions remain auditable against the version that
/// produced them.
pub const FEATURE_VERSION: u32 = 1;

/// Current model version. Increment when the model structure or update
/// logic changes, so old predictions remain auditable.
pub const MODEL_VERSION: u32 = 1;

/// The expected base rate of positive replies before any data. Encoded as a
/// Beta prior with 10 pseudo-observations — conservative, so the model starts
/// by trusting the relevance score and only diverges once it has evidence.
const PRIOR_BASE_RATE: f64 = 0.1;
const PRIOR_STRENGTH: f64 = 10.0;

/// The brain's prediction before an outreach send.
///
/// This is a bounded advisory signal. It carries provenance (model version,
/// feature version) so every prediction is auditable and reproducible.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplyPrediction {
    /// The target kind — used as the hierarchy grouping key.
    pub target_kind: String,
    /// The target identifier — the specific curator, station, or publication.
    pub target_id: String,
    /// P(positive reply) — the primary signal for ranking. In [0, 1].
    pub probability: f64,
    /// Posterior standard deviation — uncertainty for calibration.
    pub uncertainty: f64,
    /// Observation count supporting this prediction. Low confidence means
    /// the prediction is mostly prior, not data.
    pub confidence: u32,
    /// Model version (for provenance/audit).
    pub model_version: u32,
    /// Feature version (for provenance/audit)
    pub feature_version: u32,
}

/// The observed outcome of an outreach send.
///
/// The model updates from these records. The `observed_positive` field is the
/// binary label; the `disposition` is kept for audit so a human can inspect
/// why a particular observation was classified as positive or not.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplyOutcome {
    /// The target kind the prediction was made for.
    pub target_kind: String,
    /// The target identifier the prediction was made for.
    pub target_id: String,
    /// True if the reply disposition was "positive".
    pub observed_positive: bool,
    /// The actual disposition (for audit).
    pub disposition: String,
}

/// Hierarchical Beta-Bernoulli model for P(positive reply | kind, target).
///
/// This is the top-level model the brain loads at cycle start and updates
/// from observed outcomes. It wraps [`HierarchicalBetaPosterior`] with
/// versioning and a fail-cold-start interface.
///
/// # Additive and reversible
///
/// An empty model (default state) returns the global prior for every
/// prediction. The global prior is a conservative base rate, so ranking by
/// P(reply) with no data produces the same ordering as ranking by
/// `relevance_basis_points` (all targets get the same probability, the
/// tiebreaker is relevance). Deleting the model returns the system to
/// exactly this behavior.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplyProbabilityModel {
    /// The hierarchical posterior — global → kind → target.
    pub posterior: HierarchicalBetaPosterior,
    /// Model version (for provenance/audit).
    pub model_version: u32,
    /// Feature version (for provenance/audit).
    pub feature_version: u32,
}

impl Default for ReplyProbabilityModel {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplyProbabilityModel {
    /// Creates a new model with the conservative default prior.
    #[must_use]
    pub fn new() -> Self {
        Self {
            posterior: HierarchicalBetaPosterior::new(BetaPosterior::from_mean(
                PRIOR_BASE_RATE,
                PRIOR_STRENGTH,
            )),
            model_version: MODEL_VERSION,
            feature_version: FEATURE_VERSION,
        }
    }

    /// Predicts P(positive reply) for a target.
    ///
    /// Returns a [`ReplyPrediction`] with the probability, uncertainty,
    /// confidence, and provenance. The prediction uses hierarchical partial
    /// pooling: a target with no data returns the kind-level posterior, and
    /// a kind with no data returns the global prior.
    ///
    /// This method never panics. Invalid inputs (NaN, empty strings) produce
    /// a prediction equal to the global prior — fail-closed.
    #[must_use]
    pub fn predict(&self, target_kind: &str, target_id: &str) -> ReplyPrediction {
        let kind_str = if target_kind.is_empty() {
            None
        } else {
            Some(target_kind)
        };
        let target_str = if target_id.is_empty() {
            None
        } else {
            Some(target_id)
        };
        let (probability, variance) = self.posterior.predict_for_target(kind_str, target_str);
        let uncertainty = variance.sqrt();
        let confidence = self.posterior.target_confidence(target_id);
        ReplyPrediction {
            target_kind: target_kind.to_owned(),
            target_id: target_id.to_owned(),
            probability: probability.clamp(0.0, 1.0),
            uncertainty,
            confidence,
            model_version: self.model_version,
            feature_version: self.feature_version,
        }
    }

    /// Updates the model from one observed outcome.
    ///
    /// The observation updates the global posterior, the kind-level posterior
    /// (if the kind is provided), and the target-level posterior (if the
    /// target is provided). Each child's prior is the pre-update parent —
    /// this avoids double-counting.
    pub fn update(&mut self, outcome: &ReplyOutcome) {
        if outcome.target_kind.is_empty() && outcome.target_id.is_empty() {
            return;
        }
        let kind = if outcome.target_kind.is_empty() {
            None
        } else {
            Some(outcome.target_kind.as_str())
        };
        let target = if outcome.target_id.is_empty() {
            None
        } else {
            Some(outcome.target_id.as_str())
        };
        self.posterior
            .update(kind, target, outcome.observed_positive);
    }

    /// Updates the model from multiple observed outcomes.
    pub fn update_all(&mut self, outcomes: &[ReplyOutcome]) {
        for outcome in outcomes {
            self.update(outcome);
        }
    }

    /// Returns the global base rate — the prior P(positive reply) before any
    /// kind or target data. Used for observability and calibration.
    #[must_use]
    pub fn global_base_rate(&self) -> f64 {
        self.posterior.global.mean()
    }

    /// Returns the number of observations at the global level.
    #[must_use]
    pub fn global_confidence(&self) -> u32 {
        self.posterior.global.confidence()
    }

    /// Returns the number of observations for a specific kind.
    #[must_use]
    pub fn kind_confidence(&self, kind: &str) -> u32 {
        self.posterior.kind_confidence(kind)
    }

    /// Returns the number of observations for a specific target.
    #[must_use]
    pub fn target_confidence(&self, target_id: &str) -> u32 {
        self.posterior.target_confidence(target_id)
    }

    /// Returns true if the model has no observations at all — a cold start.
    /// Used to decide whether to fall back to relevance ranking entirely.
    #[must_use]
    pub fn is_cold_start(&self) -> bool {
        self.posterior.global.confidence() == 0
    }
}

/// Extracts the target key for the reply model from a target identifier.
///
/// The target key is the grouping key for the per-target posterior level. It
/// must be stable across cycles for the same target, so the model accumulates
/// evidence correctly. The target's UUID (as a string) is the natural key.
#[must_use]
pub fn target_key(target_id: &str) -> String {
    target_id.to_owned()
}

/// Extracts the kind key for the reply model from a target kind string.
///
/// The kind key is the grouping key for the per-kind posterior level. It uses
/// the same string representation as `OutreachTargetKind::as_str()`.
#[must_use]
pub fn kind_key(kind: &str) -> String {
    kind.to_owned()
}

/// Converts an outreach reply disposition to a binary label.
///
/// `positive` → `true`, everything else → `false`. This is the label the
/// Beta-Bernoulli model learns from. The actual disposition string is kept
/// in the [`ReplyOutcome`] for audit.
#[must_use]
pub fn disposition_to_label(disposition: &str) -> bool {
    disposition == "positive"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_start_returns_global_prior() {
        let model = ReplyProbabilityModel::new();
        assert!(model.is_cold_start());
        let pred = model.predict("playlist", "target:abc");
        // No data → returns global prior (0.1).
        assert!(
            (pred.probability - 0.1).abs() < 1e-12,
            "cold start should return prior, got {}",
            pred.probability
        );
        assert_eq!(pred.confidence, 0);
        assert_eq!(pred.model_version, MODEL_VERSION);
        assert_eq!(pred.feature_version, FEATURE_VERSION);
    }

    #[test]
    fn update_with_positive_moves_probability_up() {
        let mut model = ReplyProbabilityModel::new();
        for _ in 0..10 {
            model.update(&ReplyOutcome {
                target_kind: "press".to_owned(),
                target_id: "target:x".to_owned(),
                observed_positive: true,
                disposition: "positive".to_owned(),
            });
        }
        let pred = model.predict("press", "target:x");
        assert!(
            pred.probability > 0.3,
            "10 positive observations should move probability well above 0.1, got {}",
            pred.probability
        );
        assert_eq!(pred.confidence, 10);
    }

    #[test]
    fn update_with_negative_moves_probability_down() {
        let mut model = ReplyProbabilityModel::new();
        for _ in 0..10 {
            model.update(&ReplyOutcome {
                target_kind: "playlist".to_owned(),
                target_id: "target:y".to_owned(),
                observed_positive: false,
                disposition: "declined".to_owned(),
            });
        }
        let pred = model.predict("playlist", "target:y");
        assert!(
            pred.probability < 0.1,
            "10 negative observations should move probability below 0.1, got {}",
            pred.probability
        );
    }

    #[test]
    fn different_kinds_get_different_probabilities() {
        let mut model = ReplyProbabilityModel::new();
        // Press gets positive replies.
        for _ in 0..15 {
            model.update(&ReplyOutcome {
                target_kind: "press".to_owned(),
                target_id: "target:p".to_owned(),
                observed_positive: true,
                disposition: "positive".to_owned(),
            });
        }
        // Playlist gets no replies.
        for _ in 0..15 {
            model.update(&ReplyOutcome {
                target_kind: "playlist".to_owned(),
                target_id: "target:q".to_owned(),
                observed_positive: false,
                disposition: "none".to_owned(),
            });
        }
        let press_pred = model.predict("press", "target:p");
        let playlist_pred = model.predict("playlist", "target:q");
        assert!(
            press_pred.probability > playlist_pred.probability,
            "press should have higher P(reply) than playlist, got press={} playlist={}",
            press_pred.probability,
            playlist_pred.probability
        );
    }

    #[test]
    fn target_with_no_data_uses_kind_posterior() {
        let mut model = ReplyProbabilityModel::new();
        // Give "press" kind data: target:known gets positive replies, target:other gets none.
        for _ in 0..10 {
            model.update(&ReplyOutcome {
                target_kind: "press".to_owned(),
                target_id: "target:known".to_owned(),
                observed_positive: true,
                disposition: "positive".to_owned(),
            });
        }
        for _ in 0..10 {
            model.update(&ReplyOutcome {
                target_kind: "press".to_owned(),
                target_id: "target:other".to_owned(),
                observed_positive: false,
                disposition: "none".to_owned(),
            });
        }
        let known = model.predict("press", "target:known");
        let unknown = model.predict("press", "target:unknown");
        // Known target (10 successes) should be above unknown (which uses kind posterior,
        // mixed between the two targets).
        assert!(
            known.probability > unknown.probability,
            "known target should be above unknown, got known={} unknown={}",
            known.probability,
            unknown.probability
        );
        // Unknown target in "press" should be above the cold-start prior (0.1),
        // because the kind has some positive data pulling it up.
        assert!(
            unknown.probability > 0.1,
            "unknown target in press should be above cold-start prior, got {}",
            unknown.probability
        );
    }

    #[test]
    fn empty_model_returns_to_prior() {
        // Deleting the model (default state) returns the prior for everything.
        let model = ReplyProbabilityModel::new();
        let pred = model.predict("playlist", "target:any");
        assert!((pred.probability - 0.1).abs() < 1e-12);
        assert!(model.is_cold_start());
    }

    #[test]
    fn empty_kind_or_target_falls_back_gracefully() {
        let model = ReplyProbabilityModel::new();
        let pred = model.predict("", "");
        // Empty kind and target → returns global prior.
        assert!(
            (pred.probability - 0.1).abs() < 1e-12,
            "empty inputs should return prior, got {}",
            pred.probability
        );
    }

    #[test]
    fn update_with_empty_kind_and_target_is_ignored() {
        let mut model = ReplyProbabilityModel::new();
        model.update(&ReplyOutcome {
            target_kind: String::new(),
            target_id: String::new(),
            observed_positive: true,
            disposition: "positive".to_owned(),
        });
        assert!(model.is_cold_start(), "empty update should be ignored");
    }

    #[test]
    fn prediction_provenance_carries_versions() {
        let model = ReplyProbabilityModel::new();
        let pred = model.predict("press", "target:x");
        assert_eq!(pred.model_version, MODEL_VERSION);
        assert_eq!(pred.feature_version, FEATURE_VERSION);
    }

    #[test]
    fn model_state_roundtrip_preserves_predictions() {
        let mut model = ReplyProbabilityModel::new();
        for _ in 0..5 {
            model.update(&ReplyOutcome {
                target_kind: "radio".to_owned(),
                target_id: "target:z".to_owned(),
                observed_positive: true,
                disposition: "positive".to_owned(),
            });
        }
        let json = serde_json::to_string(&model).expect("serialize");
        let restored: ReplyProbabilityModel = serde_json::from_str(&json).expect("deserialize");
        let orig_pred = model.predict("radio", "target:z");
        let restored_pred = restored.predict("radio", "target:z");
        assert!(
            (orig_pred.probability - restored_pred.probability).abs() < 1e-12,
            "roundtrip should preserve predictions: {} vs {}",
            orig_pred.probability,
            restored_pred.probability
        );
    }

    #[test]
    fn update_all_processes_multiple_outcomes() {
        let mut model = ReplyProbabilityModel::new();
        let outcomes = vec![
            ReplyOutcome {
                target_kind: "press".to_owned(),
                target_id: "target:a".to_owned(),
                observed_positive: true,
                disposition: "positive".to_owned(),
            },
            ReplyOutcome {
                target_kind: "press".to_owned(),
                target_id: "target:b".to_owned(),
                observed_positive: false,
                disposition: "declined".to_owned(),
            },
        ];
        model.update_all(&outcomes);
        assert!(!model.is_cold_start());
        assert_eq!(model.global_confidence(), 2);
    }

    #[test]
    fn disposition_to_label_correct() {
        assert!(disposition_to_label("positive"));
        assert!(!disposition_to_label("declined"));
        assert!(!disposition_to_label("do_not_contact"));
        assert!(!disposition_to_label("received"));
        assert!(!disposition_to_label("none"));
    }

    #[test]
    fn target_and_kind_keys_are_stable() {
        assert_eq!(target_key("uuid-abc"), "uuid-abc");
        assert_eq!(kind_key("playlist"), "playlist");
    }

    #[test]
    fn global_base_rate_reflects_observations() {
        let mut model = ReplyProbabilityModel::new();
        assert!((model.global_base_rate() - 0.1).abs() < 1e-12);
        for _ in 0..10 {
            model.update(&ReplyOutcome {
                target_kind: "press".to_owned(),
                target_id: "target:x".to_owned(),
                observed_positive: true,
                disposition: "positive".to_owned(),
            });
        }
        assert!(
            model.global_base_rate() > 0.1,
            "10 positive observations should move global base rate up, got {}",
            model.global_base_rate()
        );
    }
}
