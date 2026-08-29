//! Bayesian posteriors — proper Normal-Normal conjugate model.
//!
//! Replaces the old EMA + pseudo-variance (which stored raw M2 and called it
//! "variance" without dividing by n-1) with a mathematically honest
//! conjugate posterior.
//!
//! # Model
//!
//! We model fan acquisition as:
//!
//! ```text
//! y_i ~ Normal(μ, σ²)
//! μ ~ Normal(μ₀, σ₀²)   (prior)
//! ```
//!
//! After observing `y_1..y_n`, the posterior is:
//!
//! ```text
//! μ | y ~ Normal(μ_n, σ_n²)
//! ```
//!
//! where:
//!
//! ```text
//! precision_n = precision_0 + n / σ²
//! μ_n = (μ₀ * precision_0 + Σy_i / σ²) / precision_n
//! σ_n² = 1 / precision_n
//! ```
//!
//! For online updates (one observation at a time):
//!
//! ```text
//! posterior_precision = prior_precision + 1 / obs_variance
//! posterior_mean = (prior_mean * prior_precision + obs * (1/obs_variance))
//!                  / posterior_precision
//! ```
//!
//! # Why not EMA?
//!
//! The old EMA with `lr = 1/(1+confidence)` is NOT compatible with Welford's
//! algorithm. Welford assumes a running mean; EMA uses an exponentially
//! weighted mean. Mixing them produces a quantity called "variance" that is
//! neither a proper posterior variance nor a running variance.
//!
//! The conjugate model is:
//! - Mathematically honest (proper posterior, proper credible intervals)
//! - Compatible with online updates (one observation at a time)
//! - Supports non-stationarity via observation variance (recent observations
//!   can have higher variance, effectively downweighting old evidence)

use serde::Serialize;

/// A Normal-Normal conjugate posterior for one parameter (e.g. expected fans
/// for one template).
///
/// The prior is `Normal(prior_mean, prior_variance)`. After each observation,
/// the posterior is updated using the conjugate formula. The observation
/// variance represents the noise level — higher variance means the brain
/// trusts the observation less and relies more on the prior.
#[derive(Clone, Debug, Serialize)]
pub struct NormalPosterior {
    /// Posterior mean — the brain's best estimate of the parameter.
    pub mean: f64,
    /// Posterior variance — the brain's uncertainty about the parameter.
    pub variance: f64,
    /// Number of observations used to form this posterior.
    pub n: u32,
}

impl Default for NormalPosterior {
    fn default() -> Self {
        Self::prior(crate::DEFAULT_EXPECTED_FANS, crate::PRIOR_VARIANCE)
    }
}

impl NormalPosterior {
    /// Creates a prior posterior with the given mean and variance.
    #[must_use]
    pub const fn prior(mean: f64, variance: f64) -> Self {
        Self {
            mean,
            variance,
            n: 0,
        }
    }

    /// Bayesian update with one observation.
    ///
    /// Uses the conjugate Normal-Normal update:
    /// ```text
    /// posterior_precision = prior_precision + observation_precision
    /// posterior_mean = weighted average by precision
    /// ```
    ///
    /// The `observation_variance` controls how much the observation moves the
    /// posterior. Higher variance → less movement (the brain trusts the
    /// observation less). This naturally handles non-stationarity: set
    /// observation variance higher for recent observations in a volatile
    /// environment.
    pub fn update(&mut self, observation: f64, observation_variance: f64) {
        if !observation.is_finite() || observation < 0.0 || observation_variance <= 0.0 {
            return;
        }
        let prior_precision = 1.0 / self.variance;
        let obs_precision = 1.0 / observation_variance;
        let post_precision = prior_precision + obs_precision;
        self.mean = (self.mean * prior_precision + observation * obs_precision) / post_precision;
        self.variance = 1.0 / post_precision;
        self.n += 1;
    }

    /// Bayesian update with one observation, allowing **signed** (negative)
    /// values. Used by the treatment-effect model where τ = Y(1) - Y(0) can
    /// be negative (the action backfired). Same conjugate formula as
    /// [`update`](Self::update), but without the non-negativity constraint.
    pub fn update_signed(&mut self, observation: f64, observation_variance: f64) {
        if !observation.is_finite() || observation_variance <= 0.0 {
            return;
        }
        let prior_precision = 1.0 / self.variance;
        let obs_precision = 1.0 / observation_variance;
        let post_precision = prior_precision + obs_precision;
        self.mean = (self.mean * prior_precision + observation * obs_precision) / post_precision;
        self.variance = 1.0 / post_precision;
        self.n += 1;
    }

    /// Posterior standard deviation — the brain's uncertainty.
    #[must_use]
    pub fn std(&self) -> f64 {
        self.variance.max(0.0).sqrt()
    }

    /// 95% credible interval for the parameter.
    #[must_use]
    pub fn ci_95(&self) -> (f64, f64) {
        let z = 1.96;
        let std = self.std();
        (self.mean - z * std, self.mean + z * std)
    }

    /// P(parameter > 0) — probability that this action has positive effect.
    /// Uses the Normal CDF approximation.
    #[must_use]
    pub fn p_positive(&self) -> f64 {
        if self.variance <= 0.0 {
            return if self.mean > 0.0 { 1.0 } else { 0.0 };
        }
        let z = self.mean / self.std();
        // Normal CDF approximation (Abramowitz & Stegun 7.1.26).
        normal_cdf(z)
    }

    /// Expected Value of Information — how much the brain would learn from
    /// one more observation. This is a proxy for expected posterior entropy
    /// reduction.
    ///
    /// Delegates to [`crate::efe::information_gain`] so there is one
    /// canonical VoI approximation across the brain.
    #[must_use]
    pub fn value_of_information(&self) -> f64 {
        crate::efe::information_gain(self.n, self.std())
    }
}

/// Standard Normal PDF: φ(z) = exp(-z²/2) / sqrt(2π).
#[must_use]
pub fn normal_pdf(z: f64) -> f64 {
    const INV_SQRT_2PI: f64 = 0.3989422804014327;
    INV_SQRT_2PI * (-z * z / 2.0).exp()
}

/// Standard Normal CDF approximation (Abramowitz & Stegun 7.1.26).
/// Accuracy: < 1e-7.
#[must_use]
pub fn normal_cdf(z: f64) -> f64 {
    let sign = if z < 0.0 { -1.0 } else { 1.0 };
    let x = z.abs() / 2.0_f64.sqrt();
    // Coefficients for the approximation.
    let a1 = 0.254829592;
    let a2 = -0.284496736;
    let a3 = 1.421413741;
    let a4 = -1.453152027;
    let a5 = 1.061405429;
    let p = 0.3275911;
    let t = 1.0 / (1.0 + p * x);
    let erf = 1.0 - (((((a5 * t + a4) * t) + a3) * t + a2) * t + a1) * t * (-x * x).exp();
    0.5 * (1.0 + sign * erf)
}

/// A hierarchical posterior with partial pooling across grouping levels.
///
/// The brain learns at multiple levels:
/// - **Global**: across all templates/contexts (the overall fan acquisition rate)
/// - **By template**: per-worker-template (reddit-scanner, community-engager, etc.)
/// - **By subreddit type**: per-community-type (metal, prog, polish, etc.)
///
/// When a template has few observations, its posterior is shrunk toward the
/// global posterior. When it has many, it stands on its own. This is
/// hierarchical Bayesian partial pooling — the gold standard for
/// multi-level learning with unequal sample sizes.
#[derive(Clone, Debug, Default, Serialize)]
pub struct HierarchicalPosterior {
    /// The global posterior — pooled across all observations.
    pub global: NormalPosterior,
    /// Per-template posteriors.
    pub by_template: std::collections::HashMap<String, NormalPosterior>,
    /// Per-subreddit-type posteriors.
    pub by_subreddit_type: std::collections::HashMap<String, NormalPosterior>,
}

impl HierarchicalPosterior {
    /// Creates a hierarchical posterior with the given global prior.
    #[must_use]
    pub fn new(global_prior: NormalPosterior) -> Self {
        Self {
            global: global_prior,
            by_template: std::collections::HashMap::new(),
            by_subreddit_type: std::collections::HashMap::new(),
        }
    }

    /// Updates the hierarchy with one observation.
    ///
    /// The observation updates:
    /// 1. The global posterior (always)
    /// 2. The template-level posterior (if template_id is provided)
    /// 3. The subreddit-type posterior (if subreddit_type is provided)
    ///
    /// Each level uses a prior derived from its parent: the template posterior
    /// uses the global posterior as its prior, the subreddit-type posterior
    /// also uses the global posterior as its prior.
    pub fn update(
        &mut self,
        template_id: Option<&str>,
        subreddit_type: Option<&str>,
        observation: f64,
        observation_variance: f64,
    ) {
        // Update global posterior.
        self.global.update(observation, observation_variance);

        // Update template posterior, using the current global posterior as prior.
        if let Some(tid) = template_id {
            let prior = NormalPosterior::prior(self.global.mean, self.global.variance);
            let entry = self.by_template.entry(tid.to_owned()).or_insert(prior);
            entry.update(observation, observation_variance);
        }

        // Update subreddit-type posterior, using the current global posterior as prior.
        if let Some(st) = subreddit_type {
            let prior = NormalPosterior::prior(self.global.mean, self.global.variance);
            let entry = self.by_subreddit_type.entry(st.to_owned()).or_insert(prior);
            entry.update(observation, observation_variance);
        }
    }

    /// Signed update — same as [`update`](Self::update) but allows negative
    /// observations. Used by the treatment-effect model where τ can be
    /// negative.
    pub fn update_signed(
        &mut self,
        template_id: Option<&str>,
        subreddit_type: Option<&str>,
        observation: f64,
        observation_variance: f64,
    ) {
        self.global.update_signed(observation, observation_variance);
        if let Some(tid) = template_id {
            let prior = NormalPosterior::prior(self.global.mean, self.global.variance);
            let entry = self.by_template.entry(tid.to_owned()).or_insert(prior);
            entry.update_signed(observation, observation_variance);
        }
        if let Some(st) = subreddit_type {
            let prior = NormalPosterior::prior(self.global.mean, self.global.variance);
            let entry = self.by_subreddit_type.entry(st.to_owned()).or_insert(prior);
            entry.update_signed(observation, observation_variance);
        }
    }

    /// Predicts the expected value for a template + subreddit type, using
    /// partial pooling.
    ///
    /// If the template has many observations, use its posterior directly.
    /// If it has few, shrink toward the subreddit-type or global posterior.
    /// The shrinkage weight is `n_template / (n_template + SHRINKAGE_STRENGTH)`.
    #[must_use]
    pub fn predict(&self, template_id: &str, subreddit_type: Option<&str>) -> (f64, f64) {
        const SHRINKAGE_STRENGTH: f64 = 5.0;

        let template_post = self.by_template.get(template_id);
        let subreddit_post = subreddit_type.and_then(|st| self.by_subreddit_type.get(st));

        // Determine the fallback (parent) posterior.
        let parent = subreddit_post.unwrap_or(&self.global);
        let parent_mean = parent.mean;
        let parent_var = parent.variance;

        match template_post {
            Some(tp) if tp.n > 0 => {
                // Shrinkage: weight the template posterior by its confidence.
                let weight = tp.n as f64 / (tp.n as f64 + SHRINKAGE_STRENGTH);
                let mean = weight * tp.mean + (1.0 - weight) * parent_mean;
                // Variance: weighted average (lower is better).
                let variance = weight * tp.variance + (1.0 - weight) * parent_var;
                (mean, variance.max(0.01))
            }
            _ => {
                // No template data: use parent (subreddit-type or global).
                (parent_mean, parent_var.max(0.01))
            }
        }
    }

    /// Returns the posterior for a template, or the global posterior if
    /// the template has no data.
    #[must_use]
    pub fn template_posterior(&self, template_id: &str) -> NormalPosterior {
        self.by_template
            .get(template_id)
            .cloned()
            .unwrap_or_else(|| self.global.clone())
    }

    /// Returns the confidence (observation count) for a template.
    #[must_use]
    pub fn confidence(&self, template_id: &str) -> u32 {
        self.by_template.get(template_id).map(|p| p.n).unwrap_or(0)
    }
}

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
#[derive(Clone, Debug, Serialize)]
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

    /// Predicts the treatment effect τ for a template + subreddit type.
    ///
    /// Returns `(tau_mean, tau_var)` — the expected treatment effect and its
    /// variance. Positive `tau_mean` means the action increases fans;
    /// negative means it backfires.
    #[must_use]
    pub fn predict(&self, template_id: &str, subreddit_type: Option<&str>) -> (f64, f64) {
        self.effects.predict(template_id, subreddit_type)
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
        self.effects.update_signed(
            Some(template_id),
            subreddit_type,
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
        let (mean, var) = self.effects.predict(template_id, subreddit_type);
        let std = var.sqrt().max(0.1);
        let confidence = self.effects.confidence(template_id);
        (mean, std, confidence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prior_has_correct_mean_and_variance() {
        let post = NormalPosterior::prior(2.0, 4.0);
        assert_eq!(post.mean, 2.0);
        assert_eq!(post.variance, 4.0);
        assert_eq!(post.n, 0);
    }

    #[test]
    fn update_moves_mean_toward_observation() {
        let mut post = NormalPosterior::prior(2.0, 4.0);
        post.update(10.0, 4.0);
        // After one update, the mean should move toward 10.
        assert!(post.mean > 2.0 && post.mean < 10.0);
        assert_eq!(post.n, 1);
    }

    #[test]
    fn update_reduces_variance() {
        let mut post = NormalPosterior::prior(2.0, 4.0);
        let initial_var = post.variance;
        post.update(5.0, 4.0);
        assert!(
            post.variance < initial_var,
            "variance should decrease after update"
        );
    }

    #[test]
    fn many_consistent_observations_shrink_variance() {
        let mut post = NormalPosterior::prior(2.0, 4.0);
        for _ in 0..20 {
            post.update(5.0, 1.0);
        }
        assert!(
            post.std() < 0.5,
            "variance should shrink with consistent observations, got std={}",
            post.std()
        );
    }

    #[test]
    fn variable_observations_keep_variance_high() {
        let mut post = NormalPosterior::prior(2.0, 4.0);
        for i in 0..20 {
            let obs = if i % 2 == 0 { 10.0 } else { 0.0 };
            post.update(obs, 1.0);
        }
        // With alternating observations, the posterior mean should be near 5
        // and the variance should still be meaningful.
        assert!((post.mean - 5.0).abs() < 1.0);
        assert!(
            post.std() > 0.1,
            "variable observations should keep uncertainty"
        );
    }

    #[test]
    fn credible_interval_contains_mean() {
        let post = NormalPosterior::prior(5.0, 4.0);
        let (lo, hi) = post.ci_95();
        assert!(lo < 5.0 && 5.0 < hi);
    }

    #[test]
    fn p_positive_for_positive_mean() {
        let post = NormalPosterior::prior(5.0, 1.0);
        assert!(
            post.p_positive() > 0.99,
            "high mean, low variance → P(>0) ≈ 1"
        );
    }

    #[test]
    fn p_positive_for_negative_mean() {
        let post = NormalPosterior::prior(-5.0, 1.0);
        assert!(
            post.p_positive() < 0.01,
            "negative mean, low variance → P(>0) ≈ 0"
        );
    }

    #[test]
    fn p_positive_for_uncertain_posterior() {
        let post = NormalPosterior::prior(0.0, 100.0);
        assert!(
            (post.p_positive() - 0.5).abs() < 0.05,
            "zero mean, high variance → P(>0) ≈ 0.5"
        );
    }

    #[test]
    fn voi_decreases_with_confidence() {
        let mut post = NormalPosterior::prior(2.0, 4.0);
        let voi_0 = post.value_of_information();
        for _ in 0..10 {
            post.update(5.0, 1.0);
        }
        let voi_10 = post.value_of_information();
        assert!(voi_10 < voi_0, "VoI should decrease as confidence grows");
    }

    #[test]
    fn voi_increases_with_variance() {
        let low_var = NormalPosterior::prior(5.0, 0.1);
        let high_var = NormalPosterior::prior(5.0, 100.0);
        assert!(
            high_var.value_of_information() > low_var.value_of_information(),
            "higher variance → higher VoI"
        );
    }

    #[test]
    fn hierarchical_shrinks_low_confidence_toward_global() {
        let mut hier = HierarchicalPosterior::new(NormalPosterior::prior(2.0, 4.0));
        // Observe 10 fans for template "a" — high confidence.
        for _ in 0..10 {
            hier.update(Some("a"), None, 10.0, 1.0);
        }
        // Template "a" should predict near 10 (high confidence, low shrinkage).
        let (mean_a, _) = hier.predict("a", None);
        assert!(
            mean_a > 8.0,
            "high-confidence template should predict near its observations, got {mean_a}"
        );

        // Template "b" has no data → should predict near global.
        let (mean_b, _) = hier.predict("b", None);
        // Global has been updated with 10 observations of 10.0, so global mean
        // is somewhere between 2.0 and 10.0. Template "b" should be near that.
        assert!(
            mean_b > 3.0,
            "no-data template should shrink toward global, got {mean_b}"
        );
    }

    #[test]
    fn hierarchical_subreddit_type_pools() {
        let mut hier = HierarchicalPosterior::new(NormalPosterior::prior(2.0, 4.0));
        // Observe high values for "metal" subreddit type.
        for _ in 0..10 {
            hier.update(Some("a"), Some("metal"), 10.0, 1.0);
        }
        // Template "b" with "metal" subreddit type should benefit from the
        // subreddit-type pooling even though "b" has no direct observations.
        let (mean_b_metal, _) = hier.predict("b", Some("metal"));
        let (mean_b_none, _) = hier.predict("b", None);
        // The metal context should produce a higher prediction.
        assert!(
            mean_b_metal >= mean_b_none,
            "subreddit-type pooling should boost prediction for known types"
        );
    }

    #[test]
    fn invalid_observations_are_ignored() {
        let mut post = NormalPosterior::prior(2.0, 4.0);
        post.update(f64::NAN, 1.0);
        post.update(-1.0, 1.0);
        post.update(5.0, -1.0);
        assert_eq!(
            post.n, 0,
            "invalid observations should not update the posterior"
        );
    }

    #[test]
    fn update_signed_accepts_negative_observations() {
        let mut post = NormalPosterior::prior(0.0, 4.0);
        post.update_signed(-5.0, 1.0);
        assert_eq!(post.n, 1);
        assert!(
            post.mean < 0.0,
            "negative observation should move mean negative"
        );
    }

    #[test]
    fn update_signed_rejects_invalid() {
        let mut post = NormalPosterior::prior(0.0, 4.0);
        post.update_signed(f64::NAN, 1.0);
        post.update_signed(5.0, -1.0);
        assert_eq!(post.n, 0, "invalid signed observations should be ignored");
    }

    #[test]
    fn treatment_effect_posterior_starts_at_zero() {
        let tep = TreatmentEffectPosterior::new();
        let (mean, _) = tep.predict("unknown", None);
        assert!(
            (mean - 0.0).abs() < 1e-9,
            "skeptical prior should start at zero, got {mean}"
        );
        assert_eq!(tep.confidence("unknown"), 0);
    }

    #[test]
    fn treatment_effect_posterior_learns_positive_effect() {
        let mut tep = TreatmentEffectPosterior::new();
        for _ in 0..10 {
            tep.update("social-post", None, 5.0, 1.0);
        }
        let (mean, std, confidence) = tep.predict_stats("social-post", None);
        assert!(mean > 2.0, "positive τ should move mean up, got {mean}");
        assert!(std < 2.0, "variance should shrink, got std={std}");
        assert_eq!(confidence, 10);
    }

    #[test]
    fn treatment_effect_posterior_learns_negative_effect() {
        let mut tep = TreatmentEffectPosterior::new();
        for _ in 0..10 {
            tep.update("bad-template", None, -3.0, 1.0);
        }
        let (mean, _, _) = tep.predict_stats("bad-template", None);
        assert!(mean < -1.0, "negative τ should move mean down, got {mean}");
    }

    #[test]
    fn treatment_effect_posterior_pools_across_templates() {
        let mut tep = TreatmentEffectPosterior::new();
        // Template "a" has lots of data showing +5 effect.
        for _ in 0..20 {
            tep.update("a", None, 5.0, 1.0);
        }
        // Template "b" has no data → should shrink toward global.
        let (mean_b, _, _) = tep.predict_stats("b", None);
        assert!(
            mean_b > 1.0,
            "no-data template should shrink toward global positive effect, got {mean_b}"
        );
    }

    #[test]
    fn treatment_effect_posterior_subreddit_type_pools() {
        let mut tep = TreatmentEffectPosterior::new();
        for _ in 0..10 {
            tep.update("a", Some("metal"), 8.0, 1.0);
        }
        let (mean_b_metal, _, _) = tep.predict_stats("b", Some("metal"));
        let (mean_b_none, _, _) = tep.predict_stats("b", None);
        assert!(
            mean_b_metal > mean_b_none,
            "metal subreddit type should boost prediction"
        );
    }

    #[test]
    fn treatment_effect_posterior_default_is_zero() {
        let tep = TreatmentEffectPosterior::default();
        let (mean, _) = tep.predict("any", None);
        assert!(
            (mean - 0.0).abs() < 1e-9,
            "default treatment effect should be zero, got {mean}"
        );
    }
}
