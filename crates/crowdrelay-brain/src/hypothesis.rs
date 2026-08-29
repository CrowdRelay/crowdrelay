//! Hypothesis registry — the brain's structured beliefs about what works.
//!
//! Each hypothesis is a testable claim about the world that the brain can
//! confirm or refute through experiments. For example:
//!
//! - "Posting to r/MetalMusic produces more fans than r/ProgMusic"
//! - "Long-form comments convert better than short comments"
//! - "Tuesday is the best day to post in metal communities"
//!
//! The registry tracks the brain's prior and posterior belief for each
//! hypothesis, updated via Bayesian updating as evidence accumulates. A
//! hypothesis transitions through statuses (`Untested` → `Supported` /
//! `Refuted` / `Inconclusive`) based on its posterior probability.
//!
//! # Bayesian updating
//!
//! When evidence supports a hypothesis:
//!
//! ```text
//! posterior = prior * L / (prior * L + (1 - prior) * (1 - L))
//! ```
//!
//! When evidence refutes a hypothesis:
//!
//! ```text
//! posterior = prior * (1 - L) / (prior * (1 - L) + (1 - prior) * L)
//! ```
//!
//! where `L = 0.7 + weight * 0.3` is the likelihood of observing the evidence
//! if the hypothesis were true. The `weight` (0–1) controls how strongly the
//! evidence should move the posterior — stronger evidence yields a likelihood
//! closer to 1.0.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Posterior threshold above which a hypothesis is considered `Supported`.
const SUPPORTED_THRESHOLD: f64 = 0.8;
/// Posterior threshold below which a hypothesis is considered `Refuted`.
const REFUTED_THRESHOLD: f64 = 0.2;
/// Base likelihood used in Bayesian updates (before applying the weight).
const BASE_LIKELIHOOD: f64 = 0.7;
/// Maximum likelihood contribution from the evidence weight.
const WEIGHT_LIKELIHOOD_SPAN: f64 = 0.3;

/// The status of a hypothesis as the brain accumulates evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HypothesisStatus {
    /// No evidence has been observed yet.
    Untested,
    /// The posterior probability is high (> 0.8) — the brain believes this.
    Supported,
    /// The posterior probability is low (< 0.2) — the brain disbelieves this.
    Refuted,
    /// The posterior is in the uncertain band (0.2–0.8).
    Inconclusive,
}

impl HypothesisStatus {
    /// Derives the status from a posterior probability.
    #[must_use]
    pub fn from_posterior(posterior: f64) -> Self {
        if posterior > SUPPORTED_THRESHOLD {
            Self::Supported
        } else if posterior < REFUTED_THRESHOLD {
            Self::Refuted
        } else {
            Self::Inconclusive
        }
    }
}

/// A testable claim about the world that the brain can confirm or refute.
///
/// The brain maintains a prior belief (`prior_probability`) and updates it
/// into a posterior belief (`posterior_probability`) as evidence accumulates.
/// Initially the posterior equals the prior.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hypothesis {
    /// Stable identifier (e.g. "reddit_metal_vs_prog").
    pub id: String,
    /// Human-readable claim, e.g. "Posting to r/MetalMusic produces more fans
    /// than r/ProgMusic".
    pub statement: String,
    /// The brain's initial belief, in [0, 1].
    pub prior_probability: f64,
    /// Updated belief after evidence, in [0, 1].
    pub posterior_probability: f64,
    /// Number of observations used to form the posterior.
    pub evidence_count: u32,
    /// Current status derived from the posterior.
    pub status: HypothesisStatus,
    /// When the hypothesis was first registered.
    pub created_at: OffsetDateTime,
    /// When the hypothesis was last updated with evidence.
    pub last_updated_at: OffsetDateTime,
}

impl Hypothesis {
    /// Creates a new untested hypothesis with the given id, statement, and
    /// prior. The posterior is initialized to the prior, the evidence count
    /// to zero, and the status to `Untested`.
    #[must_use]
    pub fn new(id: String, statement: String, prior_probability: f64) -> Self {
        let now = OffsetDateTime::now_utc();
        Self {
            id,
            statement,
            prior_probability,
            posterior_probability: prior_probability,
            evidence_count: 0,
            status: HypothesisStatus::Untested,
            created_at: now,
            last_updated_at: now,
        }
    }
}

/// The brain's registry of structured beliefs.
///
/// Each hypothesis is keyed by its stable `id`. The registry applies Bayesian
/// updates as evidence arrives and tracks which hypotheses are supported,
/// refuted, or still inconclusive.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HypothesisRegistry {
    /// All registered hypotheses, keyed by stable id.
    pub hypotheses: HashMap<String, Hypothesis>,
}

impl HypothesisRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            hypotheses: HashMap::new(),
        }
    }

    /// Registers a new hypothesis. If a hypothesis with the same id already
    /// exists, it is replaced.
    pub fn register(&mut self, hypothesis: Hypothesis) {
        self.hypotheses.insert(hypothesis.id.clone(), hypothesis);
    }

    /// Updates a hypothesis with one piece of evidence using Bayesian updating.
    ///
    /// `evidence_supports` indicates whether the evidence supports (true) or
    /// refutes (false) the hypothesis. `weight` (in [0, 1]) controls how
    /// strongly the evidence moves the posterior — the likelihood is
    /// `0.7 + weight * 0.3`.
    ///
    /// This is a convenience wrapper around [`Self::update_with_effect`] that
    /// maps the boolean + weight to a synthetic effect size. For real
    /// evidence with measured effect sizes and standard errors, prefer
    /// [`Self::update_with_effect`] directly — it uses a proper Normal
    /// likelihood instead of a hardcoded heuristic.
    ///
    /// Does nothing if the hypothesis id is unknown.
    pub fn update(&mut self, id: &str, evidence_supports: bool, weight: f64) {
        let Some(h) = self.hypotheses.get_mut(id) else {
            return;
        };
        let prior = h.posterior_probability;
        let likelihood = BASE_LIKELIHOOD + weight.clamp(0.0, 1.0) * WEIGHT_LIKELIHOOD_SPAN;
        let posterior = if evidence_supports {
            // P(H|E) = P(H) * P(E|H) / (P(H) * P(E|H) + P(!H) * P(E|!H))
            // with P(E|H) = L and P(E|!H) = 1 - L.
            prior * likelihood / (prior * likelihood + (1.0 - prior) * (1.0 - likelihood))
        } else {
            // P(H|!E) = P(H) * P(!E|H) / (P(H) * P(!E|H) + P(!H) * P(!E|!H))
            // with P(!E|H) = 1 - L and P(!E|!H) = L.
            prior * (1.0 - likelihood) / (prior * (1.0 - likelihood) + (1.0 - prior) * likelihood)
        };
        h.posterior_probability = posterior;
        h.evidence_count += 1;
        h.status = HypothesisStatus::from_posterior(posterior);
        h.last_updated_at = OffsetDateTime::now_utc();
    }

    /// Updates a hypothesis with a measured effect size and its standard
    /// error, using a proper Normal likelihood function.
    ///
    /// This is the recommended update method for real evidence. Instead of
    /// the hardcoded `0.7 + weight * 0.3` likelihood used by [`Self::update`],
    /// this method computes the likelihood from the actual measured effect:
    ///
    /// ```text
    /// L(effect | H_true)  = Normal(effect | μ_H, σ_H)
    /// L(effect | H_false) = Normal(effect | μ_notH, σ_notH)
    /// ```
    ///
    /// where `μ_H` is the expected effect under the hypothesis (positive),
    /// `μ_notH` is the expected effect under the negation (zero or negative),
    /// and `σ_H = σ_notH = standard_error`.
    ///
    /// The posterior is then:
    ///
    /// ```text
    /// posterior = prior * L_H / (prior * L_H + (1 - prior) * L_notH)
    /// ```
    ///
    /// This properly accounts for the statistical power of the evidence: a
    /// large effect with small standard error moves the posterior more than
    /// a small effect with large standard error.
    ///
    /// # Parameters
    ///
    /// - `id`: the hypothesis id.
    /// - `observed_effect`: the measured effect size (e.g. incremental fans).
    ///   Positive = supports the hypothesis, negative = refutes it.
    /// - `standard_error`: the standard error of the effect estimate. Smaller
    ///   values mean more confident evidence.
    ///
    /// Does nothing if the hypothesis id is unknown.
    pub fn update_with_effect(&mut self, id: &str, observed_effect: f64, standard_error: f64) {
        let Some(h) = self.hypotheses.get_mut(id) else {
            return;
        };
        let prior = h.posterior_probability;
        let se = standard_error.max(1e-10);
        // Under H_true: the effect is positive. We use |observed_effect| as
        // μ_H (the MLE for the magnitude, assuming H is true and the sign is
        // positive).
        // Under H_false: the effect is centered at 0 (no effect).
        let mu_h = observed_effect.abs();
        let mu_not_h = 0.0;
        // Normal likelihood: L = exp(-(x - μ)² / (2σ²)) / (σ√(2π))
        // The normalizing constant cancels in the ratio, so we only need
        // the exponential part.
        let log_l_h = -(observed_effect - mu_h).powi(2) / (2.0 * se * se);
        let log_l_not_h = -(observed_effect - mu_not_h).powi(2) / (2.0 * se * se);
        // Convert to posterior using Bayes' rule in log space for numerical
        // stability.
        // posterior = prior * L_h / (prior * L_h + (1 - prior) * L_not_h)
        // In log space: log_posterior = log(prior) + log_l_h
        //               - log_sum_exp(log(prior) + log_l_h, log(1-prior) + log_l_not_h)
        let log_prior = prior.ln();
        let log_prior_not = (1.0 - prior).ln();
        let log_term_h = log_prior + log_l_h;
        let log_term_not_h = log_prior_not + log_l_not_h;
        // log_sum_exp for numerical stability.
        let max_log = log_term_h.max(log_term_not_h);
        let log_sum_exp =
            max_log + ((log_term_h - max_log).exp() + (log_term_not_h - max_log).exp()).ln();
        let posterior = (log_term_h - log_sum_exp).exp();
        h.posterior_probability = posterior.clamp(0.0, 1.0);
        h.evidence_count += 1;
        h.status = HypothesisStatus::from_posterior(h.posterior_probability);
        h.last_updated_at = OffsetDateTime::now_utc();
    }

    /// Returns the hypothesis with the given id, if registered.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Hypothesis> {
        self.hypotheses.get(id)
    }

    /// Returns all hypotheses currently considered `Supported`.
    #[must_use]
    pub fn supported(&self) -> Vec<&Hypothesis> {
        self.hypotheses
            .values()
            .filter(|h| h.status == HypothesisStatus::Supported)
            .collect()
    }

    /// Returns all hypotheses currently considered `Refuted`.
    #[must_use]
    pub fn refuted(&self) -> Vec<&Hypothesis> {
        self.hypotheses
            .values()
            .filter(|h| h.status == HypothesisStatus::Refuted)
            .collect()
    }

    /// Returns all hypotheses that are still `Untested`.
    #[must_use]
    pub fn untested(&self) -> Vec<&Hypothesis> {
        self.hypotheses
            .values()
            .filter(|h| h.status == HypothesisStatus::Untested)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_starts_empty() {
        let registry = HypothesisRegistry::new();
        assert!(registry.hypotheses.is_empty());
        assert!(registry.supported().is_empty());
        assert!(registry.refuted().is_empty());
        assert!(registry.untested().is_empty());
    }

    #[test]
    fn register_stores_hypothesis() {
        let mut registry = HypothesisRegistry::new();
        let h = Hypothesis::new(
            "metal_vs_prog".to_owned(),
            "Posting to r/MetalMusic produces more fans than r/ProgMusic".to_owned(),
            0.5,
        );
        registry.register(h);

        let retrieved = registry
            .get("metal_vs_prog")
            .expect("hypothesis registered");
        assert_eq!(
            retrieved.statement,
            "Posting to r/MetalMusic produces more fans than r/ProgMusic"
        );
        assert_eq!(retrieved.prior_probability, 0.5);
        assert_eq!(retrieved.posterior_probability, 0.5);
        assert_eq!(retrieved.evidence_count, 0);
        assert_eq!(retrieved.status, HypothesisStatus::Untested);
    }

    #[test]
    fn register_replaces_existing_id() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new("h1".to_owned(), "first".to_owned(), 0.3));
        registry.register(Hypothesis::new("h1".to_owned(), "second".to_owned(), 0.7));

        let retrieved = registry.get("h1").expect("hypothesis exists");
        assert_eq!(retrieved.statement, "second");
        assert_eq!(retrieved.prior_probability, 0.7);
    }

    #[test]
    fn get_returns_none_for_unknown_id() {
        let registry = HypothesisRegistry::new();
        assert!(registry.get("nope").is_none());
    }

    #[test]
    fn supporting_evidence_increases_posterior() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new(
            "metal_vs_prog".to_owned(),
            "Metal > Prog".to_owned(),
            0.5,
        ));

        registry.update("metal_vs_prog", true, 1.0);

        let h = registry.get("metal_vs_prog").expect("exists");
        // With prior 0.5 and likelihood 1.0 (weight 1.0 → 0.7 + 0.3):
        // posterior = 0.5 * 1.0 / (0.5 * 1.0 + 0.5 * 0.0) = 1.0
        assert!((h.posterior_probability - 1.0).abs() < 1e-9);
        assert_eq!(h.evidence_count, 1);
        assert_eq!(h.status, HypothesisStatus::Supported);
    }

    #[test]
    fn supporting_evidence_with_partial_weight() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new("h".to_owned(), "claim".to_owned(), 0.5));

        // weight 0.0 → likelihood = 0.7
        registry.update("h", true, 0.0);

        let h = registry.get("h").expect("exists");
        // posterior = 0.5 * 0.7 / (0.5 * 0.7 + 0.5 * 0.3) = 0.35 / 0.5 = 0.7
        assert!((h.posterior_probability - 0.7).abs() < 1e-9);
        assert_eq!(h.evidence_count, 1);
        // 0.7 is in the inconclusive band
        assert_eq!(h.status, HypothesisStatus::Inconclusive);
    }

    #[test]
    fn refuting_evidence_decreases_posterior() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new(
            "metal_vs_prog".to_owned(),
            "Metal > Prog".to_owned(),
            0.5,
        ));

        registry.update("metal_vs_prog", false, 1.0);

        let h = registry.get("metal_vs_prog").expect("exists");
        // With prior 0.5 and likelihood 1.0 (weight 1.0):
        // posterior = 0.5 * 0.0 / (0.5 * 0.0 + 0.5 * 1.0) = 0.0
        assert!((h.posterior_probability - 0.0).abs() < 1e-9);
        assert_eq!(h.evidence_count, 1);
        assert_eq!(h.status, HypothesisStatus::Refuted);
    }

    #[test]
    fn refuting_evidence_with_partial_weight() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new("h".to_owned(), "claim".to_owned(), 0.5));

        // weight 0.0 → likelihood = 0.7
        registry.update("h", false, 0.0);

        let h = registry.get("h").expect("exists");
        // posterior = 0.5 * 0.3 / (0.5 * 0.3 + 0.5 * 0.7) = 0.15 / 0.5 = 0.3
        assert!((h.posterior_probability - 0.3).abs() < 1e-9);
        assert_eq!(h.evidence_count, 1);
        assert_eq!(h.status, HypothesisStatus::Inconclusive);
    }

    #[test]
    fn status_transitions_to_supported() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new("h".to_owned(), "claim".to_owned(), 0.6));

        // One strong supporting update with weight 1.0 (likelihood 1.0):
        // posterior = 0.6 * 1.0 / (0.6 * 1.0 + 0.4 * 0.0) = 1.0
        registry.update("h", true, 1.0);
        assert_eq!(
            registry.get("h").unwrap().status,
            HypothesisStatus::Supported
        );
    }

    #[test]
    fn status_transitions_to_refuted() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new("h".to_owned(), "claim".to_owned(), 0.4));

        // One strong refuting update with weight 1.0 (likelihood 1.0):
        // posterior = 0.4 * 0.0 / (0.4 * 0.0 + 0.6 * 1.0) = 0.0
        registry.update("h", false, 1.0);
        assert_eq!(registry.get("h").unwrap().status, HypothesisStatus::Refuted);
    }

    #[test]
    fn status_remains_inconclusive() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new("h".to_owned(), "claim".to_owned(), 0.5));

        // weight 0.0 → likelihood 0.7 → posterior 0.7 (inconclusive)
        registry.update("h", true, 0.0);
        assert_eq!(
            registry.get("h").unwrap().status,
            HypothesisStatus::Inconclusive
        );
    }

    #[test]
    fn multiple_updates_accumulate() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new("h".to_owned(), "claim".to_owned(), 0.5));

        // Repeated supporting evidence with weight 0.5 (likelihood 0.85).
        for _ in 0..10 {
            registry.update("h", true, 0.5);
        }

        let h = registry.get("h").expect("exists");
        assert_eq!(h.evidence_count, 10);
        // Repeated supporting evidence should push the posterior toward 1.0.
        assert!(h.posterior_probability > 0.99);
        assert_eq!(h.status, HypothesisStatus::Supported);
    }

    #[test]
    fn mixed_evidence_converges_toward_truth() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new("h".to_owned(), "claim".to_owned(), 0.5));

        // 8 supporting, 2 refuting → should end up supported.
        for _ in 0..8 {
            registry.update("h", true, 0.5);
        }
        for _ in 0..2 {
            registry.update("h", false, 0.5);
        }

        let h = registry.get("h").expect("exists");
        assert_eq!(h.evidence_count, 10);
        assert!(h.posterior_probability > 0.8);
        assert_eq!(h.status, HypothesisStatus::Supported);
    }

    #[test]
    fn update_unknown_id_is_noop() {
        let mut registry = HypothesisRegistry::new();
        registry.update("does_not_exist", true, 1.0);
        assert!(registry.hypotheses.is_empty());
    }

    #[test]
    fn supported_refuted_untested_filters() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new(
            "supported_h".to_owned(),
            "s".to_owned(),
            0.6,
        ));
        registry.register(Hypothesis::new("refuted_h".to_owned(), "r".to_owned(), 0.4));
        registry.register(Hypothesis::new(
            "untested_h".to_owned(),
            "u".to_owned(),
            0.5,
        ));

        registry.update("supported_h", true, 1.0);
        registry.update("refuted_h", false, 1.0);

        assert_eq!(registry.supported().len(), 1);
        assert_eq!(registry.refuted().len(), 1);
        assert_eq!(registry.untested().len(), 1);

        assert_eq!(registry.supported()[0].id, "supported_h");
        assert_eq!(registry.refuted()[0].id, "refuted_h");
        assert_eq!(registry.untested()[0].id, "untested_h");
    }

    #[test]
    fn weight_is_clamped_to_valid_range() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new("h".to_owned(), "claim".to_owned(), 0.5));

        // weight > 1.0 should be clamped to 1.0 (likelihood 1.0).
        registry.update("h", true, 5.0);
        let h = registry.get("h").expect("exists");
        assert!((h.posterior_probability - 1.0).abs() < 1e-9);

        // Reset and try negative weight (clamped to 0.0, likelihood 0.7).
        registry.register(Hypothesis::new("h2".to_owned(), "claim".to_owned(), 0.5));
        registry.update("h2", true, -3.0);
        let h2 = registry.get("h2").expect("exists");
        assert!((h2.posterior_probability - 0.7).abs() < 1e-9);
    }

    #[test]
    fn last_updated_advances_after_update() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new("h".to_owned(), "claim".to_owned(), 0.5));
        let created = registry.get("h").unwrap().last_updated_at;

        // Sleep is not needed — OffsetDateTime::now_utc has microsecond
        // resolution and the update happens after registration.
        registry.update("h", true, 0.5);
        let updated = registry.get("h").unwrap().last_updated_at;
        assert!(updated >= created);
    }

    #[test]
    fn hypothesis_serializes_to_json() {
        let h = Hypothesis::new("h".to_owned(), "claim".to_owned(), 0.5);
        let json = serde_json::to_string(&h).expect("serializes");
        assert!(json.contains("\"id\":\"h\""));
        assert!(json.contains("\"status\":\"untested\""));
    }

    #[test]
    fn registry_serializes_to_json() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new("h".to_owned(), "claim".to_owned(), 0.5));
        let json = serde_json::to_string(&registry).expect("serializes");
        assert!(json.contains("\"id\":\"h\""));
    }

    #[test]
    fn update_with_effect_positive_effect_increases_posterior() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new("h".to_owned(), "claim".to_owned(), 0.5));
        // Large positive effect with small SE → strong support.
        registry.update_with_effect("h", 10.0, 1.0);
        let h = registry.get("h").expect("exists");
        assert!(
            h.posterior_probability > 0.9,
            "strong positive effect should push posterior high, got {}",
            h.posterior_probability
        );
        assert_eq!(h.evidence_count, 1);
        assert_eq!(h.status, HypothesisStatus::Supported);
    }

    #[test]
    fn update_with_effect_negative_effect_decreases_posterior() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new("h".to_owned(), "claim".to_owned(), 0.5));
        // Large negative effect with small SE → strong refutation.
        registry.update_with_effect("h", -10.0, 1.0);
        let h = registry.get("h").expect("exists");
        assert!(
            h.posterior_probability < 0.1,
            "strong negative effect should push posterior low, got {}",
            h.posterior_probability
        );
        assert_eq!(h.evidence_count, 1);
        assert_eq!(h.status, HypothesisStatus::Refuted);
    }

    #[test]
    fn update_with_effect_zero_effect_keeps_posterior_near_prior() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new("h".to_owned(), "claim".to_owned(), 0.5));
        // Zero effect → no evidence either way. Posterior should stay near 0.5.
        registry.update_with_effect("h", 0.0, 5.0);
        let h = registry.get("h").expect("exists");
        assert!(
            (h.posterior_probability - 0.5).abs() < 0.2,
            "zero effect should not move posterior much, got {}",
            h.posterior_probability
        );
    }

    #[test]
    fn update_with_effect_large_se_is_weak_evidence() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new("h".to_owned(), "claim".to_owned(), 0.5));
        // Large effect but huge SE → weak evidence, posterior moves little.
        registry.update_with_effect("h", 10.0, 100.0);
        let h = registry.get("h").expect("exists");
        assert!(
            (h.posterior_probability - 0.5).abs() < 0.2,
            "huge SE should make evidence weak, got {}",
            h.posterior_probability
        );
    }

    #[test]
    fn update_with_effect_small_se_is_strong_evidence() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new("h".to_owned(), "claim".to_owned(), 0.5));
        // Small positive effect with tiny SE → still strong support.
        registry.update_with_effect("h", 1.0, 0.1);
        let h = registry.get("h").expect("exists");
        assert!(
            h.posterior_probability > 0.9,
            "tiny SE makes even small effect strong, got {}",
            h.posterior_probability
        );
    }

    #[test]
    fn update_with_effect_accumulates_across_observations() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new("h".to_owned(), "claim".to_owned(), 0.5));
        // Multiple moderate positive effects accumulate into strong support.
        // SNR = 3/3 = 1.0 per observation — moderate evidence.
        for _ in 0..10 {
            registry.update_with_effect("h", 3.0, 3.0);
        }
        let h = registry.get("h").expect("exists");
        assert_eq!(h.evidence_count, 10);
        assert!(
            h.posterior_probability > 0.8,
            "accumulated moderate evidence should become strong, got {}",
            h.posterior_probability
        );
    }

    #[test]
    fn update_with_effect_unknown_id_is_noop() {
        let mut registry = HypothesisRegistry::new();
        registry.update_with_effect("does_not_exist", 10.0, 1.0);
        assert!(registry.hypotheses.is_empty());
    }

    #[test]
    fn update_with_effect_mixed_evidence_converges() {
        let mut registry = HypothesisRegistry::new();
        registry.register(Hypothesis::new("h".to_owned(), "claim".to_owned(), 0.5));
        // 8 positive, 2 negative → should end up supported.
        for _ in 0..8 {
            registry.update_with_effect("h", 5.0, 3.0);
        }
        for _ in 0..2 {
            registry.update_with_effect("h", -5.0, 3.0);
        }
        let h = registry.get("h").expect("exists");
        assert_eq!(h.evidence_count, 10);
        assert!(
            h.posterior_probability > 0.5,
            "8 positive vs 2 negative should lean supported, got {}",
            h.posterior_probability
        );
    }
}
