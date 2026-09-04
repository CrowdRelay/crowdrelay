//! Bayesian posteriors — conjugate models for learning.
//!
//! Two conjugate families are used:
//! - **Normal-Normal** for treatment effects (signed, can be negative).
//! - **Gamma-Poisson (Negative Binomial)** for fan counts (non-negative,
//!   over-dispersed count data).
//!
//! # Normal-Normal model (treatment effects)
//!
//! For signed quantities (treatment effects τ = Y(1) - Y(0)):
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
//!
//! Note: the Signal install path (`template_expected_signal` in `CausalModel`)
//! still uses EMA by design — Signal adoption has different noise
//! characteristics than fan counts, and the EMA's simplicity is appropriate
//! there. The fan count and treatment-effect paths use the conjugate model.

use serde::{Deserialize, Serialize};

/// A Normal-Normal conjugate posterior for one parameter (e.g. expected fans
/// for one template).
///
/// The prior is `Normal(prior_mean, prior_variance)`. After each observation,
/// the posterior is updated using the conjugate formula. The observation
/// variance represents the noise level — higher variance means the brain
/// trusts the observation less and relies more on the prior.
#[derive(Clone, Debug, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HierarchicalPosterior {
    /// The global posterior — pooled across all observations.
    pub global: NormalPosterior,
    /// Per-template posteriors.
    pub by_template: std::collections::HashMap<String, NormalPosterior>,
    /// Per-subreddit-type posteriors.
    pub by_subreddit_type: std::collections::HashMap<String, NormalPosterior>,
    /// Per-target posteriors — one specific community, curator or contact.
    ///
    /// This is the level that answers "does *this* audience work", as
    /// distinct from "does this kind of audience work". It is the last level
    /// to earn data and the first one that matters: two communities in the
    /// same genre bucket predicted identically before it existed.
    ///
    /// Defaulted on deserialize so a checkpoint written before the level
    /// existed still loads; the level then refills from evidence replay.
    #[serde(default)]
    pub by_target: std::collections::HashMap<String, NormalPosterior>,
}

impl HierarchicalPosterior {
    /// Creates a hierarchical posterior with the given global prior.
    #[must_use]
    pub fn new(global_prior: NormalPosterior) -> Self {
        Self {
            global: global_prior,
            by_template: std::collections::HashMap::new(),
            by_subreddit_type: std::collections::HashMap::new(),
            by_target: std::collections::HashMap::new(),
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
    /// uses the **pre-update** global posterior as its prior, the
    /// subreddit-type posterior also uses the pre-update global posterior as
    /// its prior. This avoids double-counting: the observation must not
    /// participate in the parent prior AND be applied to the child.
    pub fn update(
        &mut self,
        template_id: Option<&str>,
        subreddit_type: Option<&str>,
        observation: f64,
        observation_variance: f64,
    ) {
        self.update_with_target(
            template_id,
            subreddit_type,
            None,
            observation,
            observation_variance,
        );
    }

    /// Updates the hierarchy including the per-target level.
    ///
    /// The target posterior's prior is the **pre-update** subreddit-type
    /// posterior where one exists, and the pre-update global otherwise. A
    /// target therefore starts life believing what its audience type
    /// believes and moves away from it as its own evidence arrives, which is
    /// the whole point of partial pooling: three observations on one
    /// community should tilt the estimate, not replace it.
    pub fn update_with_target(
        &mut self,
        template_id: Option<&str>,
        subreddit_type: Option<&str>,
        target_key: Option<&str>,
        observation: f64,
        observation_variance: f64,
    ) {
        // Snapshot the global posterior BEFORE updating it. The child's prior
        // must be the pre-update global — otherwise the observation is
        // counted once in the global (becoming the child's prior) and again
        // in the child's update, giving early observations extra weight.
        let global_prior = (self.global.mean, self.global.variance);
        // The target's parent is its audience type, snapshotted for the same
        // reason and before the same update.
        let target_prior = subreddit_type
            .and_then(|st| self.by_subreddit_type.get(st))
            .map_or(global_prior, |p| (p.mean, p.variance));

        // Update global posterior.
        self.global.update(observation, observation_variance);

        // Update template posterior, using the PRE-update global as prior.
        if let Some(tid) = template_id {
            let prior = || NormalPosterior::prior(global_prior.0, global_prior.1);
            let entry = self.by_template.entry(tid.to_owned()).or_insert_with(prior);
            entry.update(observation, observation_variance);
        }

        // Update subreddit-type posterior, using the PRE-update global as prior.
        if let Some(st) = subreddit_type {
            let prior = || NormalPosterior::prior(global_prior.0, global_prior.1);
            let entry = self
                .by_subreddit_type
                .entry(st.to_owned())
                .or_insert_with(prior);
            entry.update(observation, observation_variance);
        }

        // Update target posterior, using the PRE-update audience type as prior.
        if let Some(tk) = target_key {
            let prior = || NormalPosterior::prior(target_prior.0, target_prior.1);
            let entry = self.by_target.entry(tk.to_owned()).or_insert_with(prior);
            entry.update(observation, observation_variance);
        }
    }

    /// Signed update — same as [`update`](Self::update) but allows negative
    /// observations. Used by the treatment-effect model where τ can be
    /// negative. Uses the pre-update global as the child prior to avoid
    /// double-counting.
    pub fn update_signed(
        &mut self,
        template_id: Option<&str>,
        subreddit_type: Option<&str>,
        observation: f64,
        observation_variance: f64,
    ) {
        self.update_signed_with_target(
            template_id,
            subreddit_type,
            None,
            observation,
            observation_variance,
        );
    }

    /// Signed update including the per-target level. Same parent-prior rule
    /// as [`update_with_target`](Self::update_with_target).
    pub fn update_signed_with_target(
        &mut self,
        template_id: Option<&str>,
        subreddit_type: Option<&str>,
        target_key: Option<&str>,
        observation: f64,
        observation_variance: f64,
    ) {
        let global_prior = (self.global.mean, self.global.variance);
        let target_prior = subreddit_type
            .and_then(|st| self.by_subreddit_type.get(st))
            .map_or(global_prior, |p| (p.mean, p.variance));
        self.global.update_signed(observation, observation_variance);
        if let Some(tid) = template_id {
            let prior = || NormalPosterior::prior(global_prior.0, global_prior.1);
            let entry = self.by_template.entry(tid.to_owned()).or_insert_with(prior);
            entry.update_signed(observation, observation_variance);
        }
        if let Some(st) = subreddit_type {
            let prior = || NormalPosterior::prior(global_prior.0, global_prior.1);
            let entry = self
                .by_subreddit_type
                .entry(st.to_owned())
                .or_insert_with(prior);
            entry.update_signed(observation, observation_variance);
        }
        if let Some(tk) = target_key {
            let prior = || NormalPosterior::prior(target_prior.0, target_prior.1);
            let entry = self.by_target.entry(tk.to_owned()).or_insert_with(prior);
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

    /// Predicts for a specific target, shrinking the target's own posterior
    /// toward the (template, subreddit-type) prediction.
    ///
    /// A target with no observations predicts exactly what
    /// [`predict`](Self::predict) predicts, so adding the level cannot move
    /// an estimate the data does not support. Once the target has its own
    /// evidence it pulls away from its audience type at the same shrinkage
    /// rate every other level uses.
    #[must_use]
    pub fn predict_for_target(
        &self,
        template_id: &str,
        subreddit_type: Option<&str>,
        target_key: Option<&str>,
    ) -> (f64, f64) {
        const SHRINKAGE_STRENGTH: f64 = 5.0;
        let (parent_mean, parent_var) = self.predict(template_id, subreddit_type);
        match target_key.and_then(|tk| self.by_target.get(tk)) {
            Some(tp) if tp.n > 0 => {
                let weight = tp.n as f64 / (tp.n as f64 + SHRINKAGE_STRENGTH);
                let mean = weight * tp.mean + (1.0 - weight) * parent_mean;
                let variance = weight * tp.variance + (1.0 - weight) * parent_var;
                (mean, variance.max(0.01))
            }
            _ => (parent_mean, parent_var),
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

    /// Returns the observation count for one target.
    #[must_use]
    pub fn target_confidence(&self, target_key: &str) -> u32 {
        self.by_target.get(target_key).map(|p| p.n).unwrap_or(0)
    }
}

// ─── Negative Binomial Posterior (Gamma-Poisson conjugate) ───────────────

/// A Gamma-Poisson conjugate posterior for count data (fan acquisitions,
/// Signal installs).
///
/// Fan counts are non-negative integers with over-dispersion (0, 0, 0, 1, 0,
/// 17, 2, 0) — the variance exceeds the mean. The Normal-Normal model assumes
/// continuous Gaussian observations with constant variance, which is
/// mathematically wrong for this data type.
///
/// # Model
///
/// The Poisson rate λ has a Gamma prior:
///
/// ```text
/// λ ~ Gamma(α, β)         (prior)
/// y ~ Poisson(λ)          (observation)
/// λ | y ~ Gamma(α + y, β + 1)   (posterior)
/// ```
///
/// For multiple observations y₁..yₙ:
///
/// ```text
/// λ | y₁..yₙ ~ Gamma(α + Σyᵢ, β + n)
/// ```
///
/// The posterior predictive (marginalizing over λ) is Negative Binomial:
///
/// ```text
/// y_pred ~ NegBin(r = α', p = β' / (β' + 1))
/// E[y_pred] = α' / β'           (= λ posterior mean)
/// Var[y_pred] = α'/β' + α'/β'²  (= mean + mean²/α', over-dispersed)
/// ```
///
/// The `dispersion` parameter controls over-dispersion: higher values mean
/// more variance relative to the mean (more heterogeneous outcomes).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NegBinPosterior {
    /// Gamma shape parameter α (posterior).
    pub alpha: f64,
    /// Gamma rate parameter β (posterior).
    pub beta: f64,
    /// Number of observations.
    pub n: u32,
}

impl Default for NegBinPosterior {
    fn default() -> Self {
        Self::prior(crate::DEFAULT_EXPECTED_FANS, 2.0)
    }
}

impl NegBinPosterior {
    /// Creates a Gamma prior with the given mean and dispersion.
    ///
    /// `mean` = α/β, `dispersion` = α (the NegBin size parameter).
    /// Higher dispersion → less over-dispersion (variance closer to mean).
    /// Lower dispersion → more over-dispersion (variance >> mean).
    #[must_use]
    pub fn prior(mean: f64, dispersion: f64) -> Self {
        let alpha = dispersion.max(0.1);
        let beta = if mean > 0.0 { alpha / mean } else { alpha };
        Self { alpha, beta, n: 0 }
    }

    /// Bayesian update with one count observation.
    ///
    /// Gamma-Poisson conjugate: α += y, β += 1.
    pub fn update(&mut self, observation: u32) {
        self.alpha += f64::from(observation);
        self.beta += 1.0;
        self.n += 1;
    }

    /// Returns the posterior predictive (mean, variance).
    ///
    /// Mean = α/β, Variance = α/β + α/β² = mean + mean²/α.
    #[must_use]
    pub fn predict(&self) -> (f64, f64) {
        let mean = self.alpha / self.beta;
        let variance = mean + mean * mean / self.alpha;
        (mean, variance)
    }

    /// Returns the posterior predictive standard deviation.
    #[must_use]
    pub fn std(&self) -> f64 {
        let (_, var) = self.predict();
        var.sqrt().max(0.1)
    }

    /// Returns the posterior mean of the rate parameter λ.
    #[must_use]
    pub fn mean(&self) -> f64 {
        self.alpha / self.beta
    }

    /// Returns the number of observations.
    #[must_use]
    pub fn confidence(&self) -> u32 {
        self.n
    }

    /// P(y > 0) — probability that an observation is non-zero.
    /// For the NegBin posterior predictive: `P(y=0) = (β/(β+1))^α`.
    #[must_use]
    pub fn p_positive(&self) -> f64 {
        let p_zero = (self.beta / (self.beta + 1.0)).powf(self.alpha);
        (1.0 - p_zero).clamp(0.0, 1.0)
    }
}

/// A hierarchical NegBin posterior with partial pooling across templates
/// and subreddit types — the count-data analogue of
/// [`HierarchicalPosterior`].
///
/// Uses the same shrinkage structure: when a template has few observations,
/// its prediction shrinks toward the global posterior. When it has many, it
/// stands on its own.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HierarchicalNegBinPosterior {
    /// The global posterior — pooled across all observations.
    pub global: NegBinPosterior,
    /// Per-template posteriors.
    pub by_template: std::collections::HashMap<String, NegBinPosterior>,
    /// Per-subreddit-type posteriors.
    pub by_subreddit_type: std::collections::HashMap<String, NegBinPosterior>,
    /// Per-target posteriors — the count-data analogue of
    /// [`HierarchicalPosterior::by_target`]. Defaulted on deserialize so
    /// checkpoints written before the level existed still load.
    #[serde(default)]
    pub by_target: std::collections::HashMap<String, NegBinPosterior>,
}

impl HierarchicalNegBinPosterior {
    /// Creates a hierarchical posterior with the given global prior.
    #[must_use]
    pub fn new(global_prior: NegBinPosterior) -> Self {
        Self {
            global: global_prior,
            by_template: std::collections::HashMap::new(),
            by_subreddit_type: std::collections::HashMap::new(),
            by_target: std::collections::HashMap::new(),
        }
    }

    /// Updates the hierarchy with one count observation.
    ///
    /// The observation updates the global posterior, the template-level
    /// posterior (if provided), and the subreddit-type posterior (if provided).
    /// Each child's prior is the **pre-update** global posterior — this avoids
    /// double-counting, where the observation would participate in the parent
    /// prior (via the global update) AND be applied to the child.
    pub fn update(
        &mut self,
        template_id: Option<&str>,
        subreddit_type: Option<&str>,
        observation: u32,
    ) {
        self.update_with_target(template_id, subreddit_type, None, observation);
    }

    /// Updates the hierarchy including the per-target level. The target's
    /// prior is the pre-update subreddit-type posterior where one exists and
    /// the pre-update global otherwise — see
    /// [`HierarchicalPosterior::update_with_target`].
    pub fn update_with_target(
        &mut self,
        template_id: Option<&str>,
        subreddit_type: Option<&str>,
        target_key: Option<&str>,
        observation: u32,
    ) {
        // Snapshot the global posterior BEFORE updating it. The child's prior
        // must be the pre-update global — otherwise the observation is
        // counted once in the global (becoming the child's prior) and again
        // in the child's update, giving early observations extra weight.
        let global_alpha = self.global.alpha;
        let global_beta = self.global.beta;
        let (target_alpha, target_beta) = subreddit_type
            .and_then(|st| self.by_subreddit_type.get(st))
            .map_or((global_alpha, global_beta), |p| (p.alpha, p.beta));

        // Update global posterior.
        self.global.update(observation);

        // Update template posterior, using the PRE-update global as prior.
        if let Some(tid) = template_id {
            let prior = || NegBinPosterior {
                alpha: global_alpha,
                beta: global_beta,
                n: 0,
            };
            let entry = self.by_template.entry(tid.to_owned()).or_insert_with(prior);
            entry.update(observation);
        }

        // Update subreddit-type posterior, using the PRE-update global as prior.
        if let Some(st) = subreddit_type {
            let prior = || NegBinPosterior {
                alpha: global_alpha,
                beta: global_beta,
                n: 0,
            };
            let entry = self
                .by_subreddit_type
                .entry(st.to_owned())
                .or_insert_with(prior);
            entry.update(observation);
        }

        // Update target posterior, using the PRE-update audience type as prior.
        if let Some(tk) = target_key {
            let prior = || NegBinPosterior {
                alpha: target_alpha,
                beta: target_beta,
                n: 0,
            };
            let entry = self.by_target.entry(tk.to_owned()).or_insert_with(prior);
            entry.update(observation);
        }
    }

    /// Predicts the expected count for a template + subreddit type, using
    /// partial pooling with the same shrinkage formula as
    /// [`HierarchicalPosterior`].
    ///
    /// Returns `(mean, variance)` where `mean` is the posterior mean of the
    /// rate parameter λ and `variance` is the epistemic uncertainty about λ
    /// (the posterior variance of the Gamma distribution, `α/β²`). This is
    /// the uncertainty the EFE scorer uses for epistemic value — it shrinks
    /// as more data arrives. The posterior predictive variance (which
    /// includes Poisson sampling noise) is available via
    /// [`predict_predictive`](Self::predict_predictive).
    #[must_use]
    pub fn predict(&self, template_id: &str, subreddit_type: Option<&str>) -> (f64, f64) {
        const SHRINKAGE_STRENGTH: f64 = 5.0;

        let template_post = self.by_template.get(template_id);
        let subreddit_post = subreddit_type.and_then(|st| self.by_subreddit_type.get(st));

        let parent = subreddit_post.unwrap_or(&self.global);
        let parent_mean = parent.mean();
        let parent_var = parent.alpha / (parent.beta * parent.beta);

        match template_post {
            Some(tp) if tp.n > 0 => {
                let weight = tp.n as f64 / (tp.n as f64 + SHRINKAGE_STRENGTH);
                let tp_mean = tp.mean();
                let tp_var = tp.alpha / (tp.beta * tp.beta);
                let mean = weight * tp_mean + (1.0 - weight) * parent_mean;
                let variance = weight * tp_var + (1.0 - weight) * parent_var;
                (mean, variance.max(0.01))
            }
            _ => (parent_mean, parent_var.max(0.01)),
        }
    }

    /// Predicts the expected count for a specific target, shrinking the
    /// target's own posterior toward the (template, subreddit-type)
    /// prediction. A target with no observations predicts exactly what
    /// [`predict`](Self::predict) predicts.
    #[must_use]
    pub fn predict_for_target(
        &self,
        template_id: &str,
        subreddit_type: Option<&str>,
        target_key: Option<&str>,
    ) -> (f64, f64) {
        const SHRINKAGE_STRENGTH: f64 = 5.0;
        let (parent_mean, parent_var) = self.predict(template_id, subreddit_type);
        match target_key.and_then(|tk| self.by_target.get(tk)) {
            Some(tp) if tp.n > 0 => {
                let weight = tp.n as f64 / (tp.n as f64 + SHRINKAGE_STRENGTH);
                let tp_mean = tp.mean();
                let tp_var = tp.alpha / (tp.beta * tp.beta);
                let mean = weight * tp_mean + (1.0 - weight) * parent_mean;
                let variance = weight * tp_var + (1.0 - weight) * parent_var;
                (mean, variance.max(0.01))
            }
            _ => (parent_mean, parent_var),
        }
    }

    /// Returns the confidence (observation count) for a template.
    #[must_use]
    pub fn confidence(&self, template_id: &str) -> u32 {
        self.by_template.get(template_id).map(|p| p.n).unwrap_or(0)
    }

    /// Returns the observation count for one target.
    #[must_use]
    pub fn target_confidence(&self, target_key: &str) -> u32 {
        self.by_target.get(target_key).map(|p| p.n).unwrap_or(0)
    }

    /// Returns the posterior for a template, or the global posterior if
    /// the template has no data.
    #[must_use]
    pub fn template_posterior(&self, template_id: &str) -> NegBinPosterior {
        self.by_template
            .get(template_id)
            .cloned()
            .unwrap_or_else(|| self.global.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::treatment_effect::TreatmentEffectPosterior;

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
    fn treatment_effect_posterior_learns_positive_effect() {
        let mut tep = TreatmentEffectPosterior::new();
        for _ in 0..10 {
            tep.update_with_quality(
                "social-post",
                None,
                None,
                5.0,
                1.0,
                crate::evidence::EvidenceQuality::RandomizedHoldout,
            );
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
        // Observe high values for "metal" subreddit type.
        for _ in 0..10 {
            tep.update("a", Some("metal"), 8.0, 1.0);
        }
        // Observe low values for "pop" subreddit type — this moves the global
        // downward, so "metal" should be above the global average.
        for _ in 0..10 {
            tep.update("b", Some("pop"), 0.0, 1.0);
        }
        let (mean_b_metal, _, _) = tep.predict_stats("c", Some("metal"));
        let (mean_b_none, _, _) = tep.predict_stats("c", None);
        assert!(
            mean_b_metal > mean_b_none,
            "metal subreddit type should boost prediction above global, got metal={mean_b_metal} global={mean_b_none}"
        );
    }

    // ── NegBinPosterior tests ───────────────────────────────────────────

    #[test]
    fn negbin_prior_has_correct_mean() {
        let post = NegBinPosterior::prior(5.0, 2.0);
        let (mean, _) = post.predict();
        assert!((mean - 5.0).abs() < 0.001, "prior mean should be 5.0");
    }

    #[test]
    fn negbin_update_moves_mean_toward_observation() {
        let mut post = NegBinPosterior::prior(2.0, 2.0);
        // Observe 10 fans multiple times
        for _ in 0..20 {
            post.update(10);
        }
        let (mean, _) = post.predict();
        // After 20 observations of 10, mean should be close to 10
        assert!(mean > 8.0, "mean should move toward 10, got {mean}");
        assert!(mean < 12.0, "mean should not overshoot 10, got {mean}");
    }

    #[test]
    fn negbin_variance_greater_than_mean() {
        let mut post = NegBinPosterior::prior(2.0, 2.0);
        // Observe over-dispersed data: 0, 0, 0, 1, 0, 17, 2, 0
        for &y in &[0, 0, 0, 1, 0, 17, 2, 0] {
            post.update(y);
        }
        let (mean, variance) = post.predict();
        // NegBin property: variance > mean (over-dispersion)
        assert!(
            variance > mean,
            "variance ({variance}) should exceed mean ({mean}) for over-dispersed data"
        );
    }

    #[test]
    fn negbin_confidence_increases_with_observations() {
        let mut post = NegBinPosterior::prior(2.0, 2.0);
        assert_eq!(post.confidence(), 0);
        post.update(1);
        assert_eq!(post.confidence(), 1);
        post.update(5);
        assert_eq!(post.confidence(), 2);
    }

    #[test]
    fn negbin_handles_zero_observations() {
        let mut post = NegBinPosterior::prior(2.0, 2.0);
        post.update(0);
        post.update(0);
        post.update(0);
        let (mean, _) = post.predict();
        // After 3 zeros, mean should have moved toward 0
        assert!(mean < 2.0, "mean should decrease after zeros, got {mean}");
    }

    // ── HierarchicalNegBinPosterior tests ───────────────────────────────

    #[test]
    fn hierarchical_negbin_shrinks_low_confidence_toward_global() {
        let mut hier = HierarchicalNegBinPosterior::new(NegBinPosterior::prior(3.0, 2.0));
        // Give the global posterior lots of data at mean=5
        for _ in 0..50 {
            hier.update(None, None, 5);
        }
        // One observation for a template at 20
        hier.update(Some("rare-template"), None, 20);
        let (mean, _) = hier.predict("rare-template", None);
        // With only 1 observation, should shrink toward global (5), not stay at 20
        assert!(
            mean < 15.0,
            "low-confidence template should shrink toward global, got {mean}"
        );
        assert!(
            mean > 5.0,
            "but should be pulled somewhat toward 20, got {mean}"
        );
    }

    #[test]
    fn hierarchical_negbin_high_confidence_stands_alone() {
        let mut hier = HierarchicalNegBinPosterior::new(NegBinPosterior::prior(3.0, 2.0));
        // Give the global posterior data at mean=5
        for _ in 0..20 {
            hier.update(None, None, 5);
        }
        // Give a template lots of data at mean=15
        for _ in 0..50 {
            hier.update(Some("confident-template"), None, 15);
        }
        let (mean, _) = hier.predict("confident-template", None);
        // With 50 observations, should be close to 15, not shrunk toward 5
        assert!(
            mean > 12.0,
            "high-confidence template should stand alone, got {mean}"
        );
    }

    #[test]
    fn hierarchical_negbin_predict_unknown_template_uses_global() {
        let mut hier = HierarchicalNegBinPosterior::new(NegBinPosterior::prior(3.0, 2.0));
        for _ in 0..10 {
            hier.update(None, None, 7);
        }
        let (mean, _) = hier.predict("unknown-template", None);
        // Unknown template should use global posterior
        assert!(
            (mean - 7.0).abs() < 2.0,
            "unknown template should use global, got {mean}"
        );
    }

    // ── Double-counting regression tests (P0.1) ───────────────────────────
    //
    // The hierarchical update must use the PRE-update global as the child's
    // prior. If it uses the post-update global, the observation is counted
    // twice — once in the global (which becomes the child's prior) and again
    // in the child's update. These tests verify the fix.

    #[test]
    fn hierarchical_no_double_counting_normal() {
        // With one observation of y=10 for template "a":
        // CORRECT: global prior = (2.0, 4.0), global post = update((2,4), 10, 1)
        //          template "a" prior = (2.0, 4.0), template "a" post = update((2,4), 10, 1)
        // WRONG (double-count): template "a" prior = global_post, then update again
        //   → template "a" would be closer to 10 than it should be.
        let mut hier = HierarchicalPosterior::new(NormalPosterior::prior(2.0, 4.0));
        hier.update(Some("a"), None, 10.0, 1.0);

        // The template posterior should be the result of updating the
        // PRE-update global prior (2.0, 4.0) with observation 10.0, var 1.0.
        // prior_precision = 1/4 = 0.25, obs_precision = 1/1 = 1.0
        // post_precision = 1.25
        // post_mean = (2.0 * 0.25 + 10.0 * 1.0) / 1.25 = (0.5 + 10) / 1.25 = 8.4
        let template_post = hier.by_template.get("a").unwrap();
        assert!(
            (template_post.mean - 8.4).abs() < 0.01,
            "template posterior should use pre-update global prior, got mean={}",
            template_post.mean
        );
        // If double-counted, the prior would be the post-update global
        // (mean=8.4, var=0.8), and updating with 10.0 would give:
        // precision = 1/0.8 + 1 = 2.25, mean = (8.4*1.25 + 10) / 2.25 = 9.156
        // So 8.4 vs 9.156 is the distinguishing assertion.
    }

    #[test]
    fn hierarchical_no_double_counting_signed() {
        let mut hier = HierarchicalPosterior::new(NormalPosterior::prior(0.0, 4.0));
        hier.update_signed(Some("a"), None, 5.0, 1.0);

        // Same math: prior (0, 4), obs 5, var 1
        // post_mean = (0*0.25 + 5*1.0) / 1.25 = 4.0
        let template_post = hier.by_template.get("a").unwrap();
        assert!(
            (template_post.mean - 4.0).abs() < 0.01,
            "signed template posterior should use pre-update global prior, got mean={}",
            template_post.mean
        );
    }

    #[test]
    fn hierarchical_no_double_counting_negbin() {
        // NegBin: prior(mean=3.0, dispersion=2.0) → alpha=2.0, beta=2/3.
        // Observe y=10.
        // CORRECT: template prior = (alpha=2, beta=2/3), then update:
        //   alpha = 2+10 = 12, beta = 2/3+1 = 5/3
        // WRONG (double-count): global updated first → alpha=12, beta=5/3
        //   template prior = (12, 5/3), then update: alpha=22, beta=8/3
        let mut hier = HierarchicalNegBinPosterior::new(NegBinPosterior::prior(3.0, 2.0));
        hier.update(Some("a"), None, 10);

        let template_post = hier.by_template.get("a").unwrap();
        // Correct: alpha = 2 + 10 = 12, beta = 2/3 + 1 ≈ 1.667
        assert_eq!(
            template_post.alpha, 12.0,
            "template alpha should be 12 (pre-update global + obs), got {}",
            template_post.alpha
        );
        assert!(
            (template_post.beta - 5.0 / 3.0).abs() < 0.01,
            "template beta should be 5/3 (pre-update global + 1), got {}",
            template_post.beta
        );
        // If double-counted: alpha would be 22, beta would be 8/3.
    }

    // ── Per-target level ──

    #[test]
    fn target_with_no_data_predicts_exactly_what_the_template_predicts() {
        // Adding the level must not move an estimate the data does not
        // support: an unobserved target inherits its parent untouched.
        let mut h = HierarchicalNegBinPosterior::new(NegBinPosterior::prior(2.0, 1.0));
        h.update(Some("community-engager"), Some("metal"), 4);
        let parent = h.predict("community-engager", Some("metal"));
        let target = h.predict_for_target("community-engager", Some("metal"), Some("community:x"));
        assert!((parent.0 - target.0).abs() < 1e-12);
        assert!((parent.1 - target.1).abs() < 1e-12);
    }

    #[test]
    fn two_targets_in_one_genre_bucket_diverge_once_they_have_evidence() {
        // This is the whole point of the level. Before it existed, r/good and
        // r/bad shared a subreddit_type and therefore shared a prediction.
        let mut h = HierarchicalNegBinPosterior::new(NegBinPosterior::prior(2.0, 1.0));
        for _ in 0..8 {
            h.update_with_target(
                Some("community-engager"),
                Some("metal"),
                Some("community:good"),
                9,
            );
            h.update_with_target(
                Some("community-engager"),
                Some("metal"),
                Some("community:bad"),
                0,
            );
        }
        let good = h
            .predict_for_target("community-engager", Some("metal"), Some("community:good"))
            .0;
        let bad = h
            .predict_for_target("community-engager", Some("metal"), Some("community:bad"))
            .0;
        assert!(
            good > bad + 1.0,
            "targets must separate: good={good} bad={bad}"
        );
    }

    #[test]
    fn target_shrinks_toward_its_audience_type_on_thin_evidence() {
        // One observation must tilt the estimate, not replace it.
        let mut h = HierarchicalNegBinPosterior::new(NegBinPosterior::prior(2.0, 1.0));
        for _ in 0..20 {
            h.update(Some("community-engager"), Some("metal"), 5);
        }
        let parent = h.predict("community-engager", Some("metal")).0;
        h.update_with_target(
            Some("community-engager"),
            Some("metal"),
            Some("community:new"),
            0,
        );
        let target = h
            .predict_for_target("community-engager", Some("metal"), Some("community:new"))
            .0;
        assert!(
            target < parent,
            "a zero-fan observation should pull the target down"
        );
        assert!(
            target > parent / 2.0,
            "one observation must not overwhelm the audience type: target={target} parent={parent}"
        );
    }

    #[test]
    fn target_confidence_counts_only_that_targets_observations() {
        let mut h = HierarchicalPosterior::new(NormalPosterior::prior(0.0, 4.0));
        h.update_signed_with_target(Some("t"), Some("metal"), Some("community:a"), 1.0, 1.0);
        h.update_signed_with_target(Some("t"), Some("metal"), Some("community:b"), 1.0, 1.0);
        h.update_signed_with_target(Some("t"), Some("metal"), Some("community:a"), 1.0, 1.0);
        assert_eq!(h.target_confidence("community:a"), 2);
        assert_eq!(h.target_confidence("community:b"), 1);
        assert_eq!(h.target_confidence("community:missing"), 0);
        // The template level still sees every observation.
        assert_eq!(h.confidence("t"), 3);
    }

    #[test]
    fn target_level_survives_a_checkpoint_written_without_it() {
        // Old checkpoints must load, then refill from replay.
        let json =
            r#"{"global":{"alpha":2.0,"beta":1.0,"n":0},"by_template":{},"by_subreddit_type":{}}"#;
        let h: HierarchicalNegBinPosterior =
            serde_json::from_str(json).expect("legacy checkpoint should deserialize");
        assert!(h.by_target.is_empty());
    }
}
