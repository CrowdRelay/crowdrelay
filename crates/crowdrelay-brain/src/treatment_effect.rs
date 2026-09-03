//! Posterior over the treatment effect τ = Y(1) - Y(0).
//!
//! Split from `bayesian.rs` because it is a different question from the
//! outcome model: `bayesian` holds the conjugate families and the pooling
//! machinery, this holds the one quantity the brain ranks actions by.

use serde::{Deserialize, Serialize};

use crate::bayesian::{HierarchicalPosterior, NormalPosterior, normal_cdf};

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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TreatmentEffectPosterior {
    /// Hierarchical posterior for τ, same structure as the fan posterior.
    pub effects: HierarchicalPosterior,
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
        self.effects.update_signed_with_target(
            Some(template_id),
            subreddit_type,
            target_key,
            observed_tau,
            observation_variance,
        );
    }

    /// Returns the confidence (observation count) for a template's treatment
    /// effect.
    #[must_use]
    pub fn confidence(&self, template_id: &str) -> u32 {
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
        let confidence = self.effects.confidence(template_id);
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
