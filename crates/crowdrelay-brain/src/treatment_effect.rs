//! Posterior over the treatment effect τ = Y(1) - Y(0).
//!
//! Split from `bayesian.rs` because it is a different question from the
//! outcome model: `bayesian` holds the conjugate families and the pooling
//! machinery, this holds the one quantity the brain ranks actions by.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::bayesian::{HierarchicalPosterior, NormalPosterior, normal_cdf};
use crate::evidence::EvidenceQuality;

/// Posterior over the treatment effect τ = Y(1) - Y(0) for a template.
///
/// Uses a Normal-Normal conjugate model on the **signed** treatment effect,
/// with the same hierarchical partial pooling structure as the outcome model
/// ([`HierarchicalPosterior`]). The key difference: τ can be negative (the
/// action backfired), so updates use [`NormalPosterior::update_signed`].
///
/// # Prior
///
/// The prior is centered at 0.0 (no effect) with [`crate::PRIOR_VARIANCE`].
/// This is a skeptical prior — the brain starts believing actions have no
/// effect until evidence proves otherwise.
///
/// # Ranking
///
/// The brain ranks templates by `τ(x)` — the heterogeneous treatment effect
/// for context `x`. A template with `τ = +5` produces 5 incremental fans on
/// average; `τ = -3` means the action loses 3 fans. This is the causally
/// correct ranking signal, unlike the outcome model which conflates
/// correlation with causation.
/// # Confidence is quality-weighted
///
/// The switch from the outcome model to the treatment-effect model is a claim
/// that the treatment effect is *identified*, and a raw observation count
/// cannot support that claim. Counting rows meant five pre/post rows crossed
/// [`crate::MIN_TREATMENT_CONFIDENCE`] and the brain started ranking by a
/// "treatment effect" estimated with no control arm, labelling the result
/// `DecisionMode::Exploit`. Downweighting the variance was not enough: the
/// variance decides how far the estimate moves, the confidence decides
/// whether we believe it at all.
///
/// So confidence accumulates [`EvidenceQuality::weight`] rather than rows. A
/// randomized holdout contributes 1.0 and five of them still flip the switch;
/// observational evidence contributes 0.1, so it takes fifty rows — which is
/// the honest exchange rate between watching and experimenting.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TreatmentEffectPosterior {
    /// Hierarchical posterior for τ, same structure as the fan posterior.
    pub effects: HierarchicalPosterior,
    /// Quality-weighted observation mass per template. Defaulted on
    /// deserialize; a checkpoint written before this existed reports zero
    /// confidence until evidence replay refills it, which is the safe
    /// direction — the brain falls back to the outcome model rather than
    /// trusting a treatment effect it cannot account for.
    #[serde(default)]
    pub effective_observations: HashMap<String, f64>,
}

impl Default for TreatmentEffectPosterior {
    fn default() -> Self {
        Self::new()
    }
}

impl TreatmentEffectPosterior {
    /// Creates a treatment-effect posterior with a skeptical prior (mean=0,
    /// variance=PRIOR_VARIANCE).
    #[must_use]
    pub fn new() -> Self {
        Self {
            effects: HierarchicalPosterior::new(NormalPosterior::prior(0.0, crate::PRIOR_VARIANCE)),
            effective_observations: HashMap::new(),
        }
    }

    /// Updates the treatment-effect posterior from an observed τ.
    ///
    /// `observed_tau` is the estimated treatment effect (e.g. from an IPW
    /// estimator). It can be negative. `observation_variance` is the variance
    /// of the estimate (from the IPW computation).
    pub fn update(
        &mut self,
        template_id: &str,
        subreddit_type: Option<&str>,
        observed_tau: f64,
        observation_variance: f64,
    ) {
        self.update_for_target(
            template_id,
            subreddit_type,
            None,
            observed_tau,
            observation_variance,
        );
    }

    /// Updates the treatment-effect posterior for one specific target.
    ///
    /// The treatment effect of posting to r/djent is not the treatment effect
    /// of posting to metal subreddits in general, and recording it at the
    /// target level is what lets the two diverge as evidence arrives.
    pub fn update_for_target(
        &mut self,
        template_id: &str,
        subreddit_type: Option<&str>,
        target_key: Option<&str>,
        observed_tau: f64,
        observation_variance: f64,
    ) {
        // No quality stated means the weakest one. An unlabelled observation
        // must not buy the same confidence as a randomized holdout.
        self.update_with_quality(
            template_id,
            subreddit_type,
            target_key,
            observed_tau,
            observation_variance,
            EvidenceQuality::Observational,
        );
    }

    /// Updates the posterior and accumulates quality-weighted confidence.
    ///
    /// `quality` moves the confidence, `observation_variance` moves the
    /// estimate. They are separate on purpose: strong evidence should both
    /// shift the mean further and earn the right to be believed, and a caller
    /// that already scaled its variance by quality has not thereby said
    /// anything about identification.
    pub fn update_with_quality(
        &mut self,
        template_id: &str,
        subreddit_type: Option<&str>,
        target_key: Option<&str>,
        observed_tau: f64,
        observation_variance: f64,
        quality: EvidenceQuality,
    ) {
        self.effects.update_signed_with_target(
            Some(template_id),
            subreddit_type,
            target_key,
            observed_tau,
            observation_variance,
        );
        *self
            .effective_observations
            .entry(template_id.to_owned())
            .or_insert(0.0) += quality.weight();
    }

    /// Returns the quality-weighted confidence for a template's treatment
    /// effect, floored to whole observations. Fifty observational rows read
    /// as five; five randomized ones read as five.
    #[must_use]
    pub fn confidence(&self, template_id: &str) -> u32 {
        let effective = self
            .effective_observations
            .get(template_id)
            .copied()
            .unwrap_or(0.0);
        // A checkpoint predating `effective_observations` has the posterior
        // but not the mass. Reporting zero is the safe reading: it falls back
        // to the outcome model until replay re-establishes the weights.
        //
        // The epsilon is not decoration. Weights are floats, so fifty
        // additions of 0.1 land on 4.999999999999998 and a bare floor reports
        // four — the count would be short by one for no reason a reader could
        // ever find.
        (effective.max(0.0) + 1e-9).floor() as u32
    }

    /// The raw number of observations behind a template's posterior,
    /// regardless of their quality. Reporting only — the decision path uses
    /// [`confidence`](Self::confidence).
    #[must_use]
    pub fn observation_count(&self, template_id: &str) -> u32 {
        self.effects.confidence(template_id)
    }

    /// Returns `(tau, std, confidence)` in a single hierarchical lookup.
    /// This is the hot path for EFE scoring.
    #[must_use]
    pub fn predict_stats(
        &self,
        template_id: &str,
        subreddit_type: Option<&str>,
    ) -> (f64, f64, u32) {
        self.predict_stats_for_target(template_id, subreddit_type, None)
    }

    /// Returns `(tau, std, confidence)` for a specific target.
    ///
    /// The confidence reported is the **template's**, not the target's. The
    /// switch between the treatment-effect model and the outcome model is a
    /// statement about whether the treatment effect is identified at all,
    /// and three observations on one community do not identify it. The
    /// target level moves the estimate; it does not license trusting it.
    #[must_use]
    pub fn predict_stats_for_target(
        &self,
        template_id: &str,
        subreddit_type: Option<&str>,
        target_key: Option<&str>,
    ) -> (f64, f64, u32) {
        let (mean, var) = self
            .effects
            .predict_for_target(template_id, subreddit_type, target_key);
        let std = var.sqrt().max(0.1);
        // The quality-weighted count, not the raw one. `self.effects.confidence`
        // is the number of rows; `self.confidence` is how much identification
        // those rows bought. Reading the former let five pre/post rows flip the
        // regime switch exactly as fast as five randomized holdouts, which is
        // the thing `effective_observations` exists to prevent.
        let confidence = self.confidence(template_id);
        (mean, std, confidence)
    }

    /// Returns P(τ > δ) — the probability that the treatment effect exceeds
    /// a meaningful threshold δ. This is the **meaningful-effect probability**,
    /// a more decision-relevant signal than just P(τ > 0).
    ///
    /// For example, if δ = 1.0 (we only care about effects that produce at
    /// least 1 durable fan), then P(τ > 1.0) tells us the probability that
    /// this template is worth dispatching at all.
    ///
    /// The posterior over τ is approximately Normal(μ, σ²), so:
    ///
    /// ```text
    /// P(τ > δ) = 1 - Φ((δ - μ) / σ)
    /// ```
    ///
    /// where Φ is the standard Normal CDF.
    #[must_use]
    pub fn p_meaningful_effect(
        &self,
        template_id: &str,
        subreddit_type: Option<&str>,
        delta: f64,
    ) -> f64 {
        let (mean, var) = self.effects.predict(template_id, subreddit_type);
        let std = var.sqrt().max(0.1);
        // Standard Normal CDF: Φ(z) = 0.5 * (1 + erf(z / sqrt(2)))
        let z = (delta - mean) / std;
        1.0 - normal_cdf(z)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MIN_TREATMENT_CONFIDENCE;

    #[test]
    fn observational_rows_cannot_buy_treatment_confidence() {
        // The defect this exists to prevent: five pre/post rows crossing the
        // switch and the brain ranking by a "treatment effect" with no
        // control arm anywhere in it.
        let mut tep = TreatmentEffectPosterior::new();
        for _ in 0..MIN_TREATMENT_CONFIDENCE {
            tep.update_with_quality(
                "community-engager",
                Some("metal"),
                None,
                2.0,
                1.0,
                EvidenceQuality::Observational,
            );
        }
        assert_eq!(tep.observation_count("community-engager"), 5);
        assert!(
            tep.confidence("community-engager") < MIN_TREATMENT_CONFIDENCE,
            "observational evidence must not flip the treatment-effect switch"
        );
    }

    /// The same guard, asserted where the decision is actually made.
    ///
    /// The test above asserts `confidence()`, and `confidence()` was not what
    /// the decision path read: `predict_stats_for_target` called
    /// `self.effects.confidence`, the raw row count. So the guard returned the
    /// right number to nobody, the test stayed green, and five observational
    /// rows flipped `use_treatment_effect` exactly as fast as five randomized
    /// holdouts. Asserting on `use_treatment_effect` is what closes that gap:
    /// it is the flag that decides whether `DecisionValue::pragmatic_value` is
    /// a treatment effect or an outcome-model prediction.
    #[test]
    fn the_quality_gate_governs_the_live_regime_switch() {
        use crate::causal_model::{CausalModel, DispatchContext};

        let switched = |quality: EvidenceQuality| {
            let mut model = CausalModel::new();
            for _ in 0..MIN_TREATMENT_CONFIDENCE {
                model.update_treatment_effect_for_target("t", None, None, 2.0, 1.0, quality);
            }
            model
                .predict_stats_with_treatment_for_target("t", None, &DispatchContext::default())
                .use_treatment_effect
        };

        assert!(
            !switched(EvidenceQuality::Observational),
            "five observational rows must not put the brain on the treatment effect"
        );
        assert!(
            switched(EvidenceQuality::RandomizedHoldout),
            "five randomized holdouts must, or the switch can never fire at all"
        );
    }

    #[test]
    fn randomized_holdouts_reach_the_switch_at_the_stated_count() {
        let mut tep = TreatmentEffectPosterior::new();
        for _ in 0..MIN_TREATMENT_CONFIDENCE {
            tep.update_with_quality(
                "community-engager",
                Some("metal"),
                None,
                2.0,
                1.0,
                EvidenceQuality::RandomizedHoldout,
            );
        }
        assert_eq!(
            tep.confidence("community-engager"),
            MIN_TREATMENT_CONFIDENCE
        );
    }

    #[test]
    fn fifty_observational_rows_read_as_five() {
        // The exchange rate is the point: observational evidence is worth
        // something, just an order of magnitude less.
        let mut tep = TreatmentEffectPosterior::new();
        for _ in 0..50 {
            tep.update_with_quality("t", None, None, 1.0, 1.0, EvidenceQuality::Observational);
        }
        assert_eq!(tep.confidence("t"), 5);
    }

    #[test]
    fn unqualified_updates_are_treated_as_observational() {
        let mut tep = TreatmentEffectPosterior::new();
        for _ in 0..5 {
            tep.update("t", None, 1.0, 1.0);
        }
        assert_eq!(tep.confidence("t"), 0);
        assert_eq!(tep.observation_count("t"), 5);
    }

    #[test]
    fn confidence_is_per_template() {
        let mut tep = TreatmentEffectPosterior::new();
        for _ in 0..5 {
            tep.update_with_quality(
                "a",
                None,
                None,
                1.0,
                1.0,
                EvidenceQuality::RandomizedHoldout,
            );
        }
        assert_eq!(tep.confidence("a"), 5);
        assert_eq!(tep.confidence("b"), 0);
    }

    #[test]
    fn a_checkpoint_without_weights_reports_no_confidence() {
        // Safe direction: fall back to the outcome model until replay
        // re-establishes the weights, rather than trusting a treatment effect
        // whose provenance was not recorded.
        let json = r#"{"effects":{"global":{"mean":0.0,"variance":4.0,"n":9},"by_template":{},"by_subreddit_type":{}}}"#;
        let tep: TreatmentEffectPosterior =
            serde_json::from_str(json).expect("legacy checkpoint should deserialize");
        assert_eq!(tep.confidence("anything"), 0);
    }
}
