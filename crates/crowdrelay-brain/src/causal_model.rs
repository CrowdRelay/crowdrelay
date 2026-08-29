//! Causal Model — P(incremental_fan | template, context).
//!
//! The brain's causal model predicts how many incremental fans a worker
//! dispatch will produce, given the template and context features. It uses
//! a Gamma-Poisson (Negative Binomial) conjugate model for count data with
//! over-dispersion, replacing the old Normal-Normal model.
//!
//! # Architecture
//!
//! - **Hierarchical posterior**: global + per-template + per-subreddit-type
//!   partial pooling. Low-confidence templates shrink toward the global mean.
//! - **Context adjustments**: a learned log-linear GLM modulates the base
//!   prediction based on event proximity and growth trend.
//! - **Independent Signal install learning**: Signal adoption has different
//!   drivers than fan acquisition, so it's learned separately.
//! - **Proper variance**: `HierarchicalNegBinPosterior` gives mathematically
//!   honest posterior variance, credible intervals, and P(positive) for the
//!   count outcome; `TreatmentEffectPosterior` (Normal-Normal) does the same
//!   for the continuous treatment effect τ.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::bayesian::{HierarchicalNegBinPosterior, NegBinPosterior, TreatmentEffectPosterior};
use crate::context_effect::ContextGLM;
use crate::world_model::GrowthTrend;

/// Context features that the causal model uses to predict fan acquisition
/// outcomes. These are the variables the brain believes influence whether
/// a dispatch will produce new fans.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DispatchContext {
    /// Days until the nearest upcoming event, if any. Event proximity
    /// boosts fan acquisition potential.
    pub days_to_event: Option<u32>,
    /// The current fan growth trend. Stagnant situations are harder to
    /// grow out of; accelerating ones are easier.
    pub fan_growth_trend: GrowthTrend,
    /// The type of subreddit/community being targeted (e.g. "metal",
    /// "prog", "polish"). Used for context-level prediction.
    pub subreddit_type: Option<String>,
    /// The post format being used (e.g. "text", "link", "video").
    pub post_format: Option<String>,
    /// Time of day as basis points (0–10_000, fraction of 24h).
    pub time_of_day_bps: u16,
    /// How novel this dispatch context is compared to past dispatches
    /// (0–10_000). Higher = more novel.
    pub community_novelty_bps: u16,
}

/// The brain's prediction before a dispatch. Records what the brain
/// expected to happen, so that after measurement the prediction error
/// can be computed and fed back into the causal model.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DispatchPrediction {
    /// The worker template that was dispatched.
    pub template_id: String,
    /// Expected new fans from this dispatch.
    pub expected_new_fans: f64,
    /// Expected new Signal installs from this dispatch.
    pub expected_signal_installs: f64,
    /// The context features that informed this prediction.
    pub context: DispatchContext,
}

/// The measured outcome of a dispatch, paired with the prediction that
/// was made before it. The prediction errors (observed - expected) are
/// the dopamine signals that drive learning.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PredictionOutcome {
    /// The prediction that was made before the dispatch.
    pub prediction: DispatchPrediction,
    /// New fans actually observed in the measurement window.
    pub observed_new_fans: f64,
    /// Signal installs actually observed in the measurement window.
    pub observed_signal_installs: f64,
    /// The dopamine signal for fans: observed - expected.
    /// Positive = better than expected (surprise reward).
    /// Negative = worse than expected (disappointment).
    pub fan_prediction_error: f64,
    /// The dopamine signal for Signal installs.
    pub signal_prediction_error: f64,
}

impl PredictionOutcome {
    /// Computes the outcome from a prediction and observed values.
    #[must_use]
    pub fn from_observation(
        prediction: DispatchPrediction,
        observed_new_fans: f64,
        observed_signal_installs: f64,
    ) -> Self {
        Self {
            fan_prediction_error: observed_new_fans - prediction.expected_new_fans,
            signal_prediction_error: observed_signal_installs - prediction.expected_signal_installs,
            prediction,
            observed_new_fans,
            observed_signal_installs,
        }
    }
}

/// The default expected fans per dispatch when no data is available.
/// A prior of 2.0 means the brain expects ~2 new fans per worker dispatch
/// — optimistic enough to keep dispatching, conservative enough to be
/// realistic about free-channel fan acquisition.
pub const DEFAULT_EXPECTED_FANS: f64 = 2.0;

/// The default expected Signal installs per dispatch. Signal conversion
/// is harder than fan acquisition — most fans don't install the app.
pub const DEFAULT_EXPECTED_SIGNAL: f64 = 0.2;

/// The prior variance used by the Normal-Normal treatment-effect and
/// strategy-learning posteriors. The fan outcome prior is set separately
/// via `NegBinPosterior::prior(DEFAULT_EXPECTED_FANS, 1.0)` in
/// [`CausalModel::new`].
pub const PRIOR_VARIANCE: f64 = 4.0;

/// Minimum number of paired treatment/control observations before the brain
/// trusts the treatment-effect posterior over the outcome model. Below this,
/// the brain falls back to the outcome model P(Y|action,context) because the
/// treatment-effect estimate is too noisy.
pub const MIN_TREATMENT_CONFIDENCE: u32 = 5;

/// The meaningful-effect threshold δ. The brain only considers a template
/// "worth dispatching" if P(τ > δ) is high enough. δ = 1.0 means we require
/// at least 1 durable fan of treatment effect to consider the action
/// meaningful. This prevents the brain from optimizing for tiny effects
/// that are statistically significant but practically irrelevant.
pub const MEANINGFUL_EFFECT_THRESHOLD: f64 = 1.0;

/// The hysteresis band for the treatment-effect switch. Once the brain
/// switches to the treatment-effect model, it requires the confidence to
/// drop below `MIN_TREATMENT_CONFIDENCE - HYSTERESIS_BAND` before switching
/// back to the outcome model. This prevents oscillation near the threshold.
pub const HYSTERESIS_BAND: u32 = 2;

/// The brain's causal model: P(incremental_fan | template, context).
///
/// Uses a `HierarchicalNegBinPosterior` for proper Bayesian learning with partial
/// pooling across templates and subreddit types. The hierarchical structure
/// means:
/// - Templates with many observations stand on their own.
/// - Templates with few observations shrink toward the global mean.
/// - Subreddit-type multipliers are learned and pooled.
///
/// Context adjustments (event proximity, growth trend) are applied via a
/// learned log-linear GLM on top of the posterior mean.
///
/// # Treatment-effect model
///
/// In addition to the outcome model P(Y|action,context), the brain maintains
/// a treatment-effect posterior P(τ|context) where τ = Y(1) - Y(0). This is
/// the causally correct ranking signal. When enough experiment data has
/// accumulated ([`MIN_TREATMENT_CONFIDENCE`] paired observations), the brain
/// uses τ as the primary ranking signal. Before that, it falls back to the
/// outcome model.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CausalModel {
    /// Hierarchical NegBin posterior for fan acquisition (outcome model).
    /// Uses Gamma-Poisson conjugate model for count data with over-dispersion.
    pub fans: HierarchicalNegBinPosterior,
    /// Treatment-effect posterior P(τ|context) for Y14 (14-day incremental).
    /// Primary ranking signal when confidence is high; falls back to the
    /// outcome model when low.
    pub treatment_effects: TreatmentEffectPosterior,
    /// Treatment-effect posterior P(τ|context) for Y30 (30-day durable).
    /// The North Star target — fans still active after 30 days. Uses the
    /// same hierarchical structure as `treatment_effects` but learns from
    /// the Y30 outcome, which arrives later.
    pub treatment_effects_y30: TreatmentEffectPosterior,
    /// Per-template Signal install EMA. Learned independently from fan
    /// counts because Signal adoption has different drivers.
    pub template_expected_signal: HashMap<String, f64>,
    /// Calibration tracker for Y14 (14-day incremental) predictions.
    /// Learns the mapping between predicted and observed incremental fans
    /// and corrects future predictions to reduce bias. This is the early
    /// leading signal — available 14 days after dispatch.
    pub calibration: crate::calibration::CalibrationTracker,
    /// Calibration tracker for Y30 (30-day durable) predictions. Learns
    /// the mapping between predicted and observed durable fans. This is
    /// the North Star target — fans still active after 30 days. It arrives
    /// later than Y14 but is the ultimate quality signal.
    pub calibration_y30: crate::calibration::CalibrationTracker,
    /// Learned context effects — a hierarchical log-linear GLM that replaces
    /// hardcoded multipliers (×1.5 event, ×0.8 stagnant, etc.) with learned
    /// coefficients in log space. Prevents multiplicative explosion and
    /// learns from partial residuals instead of confounded ratios.
    pub context_effects: ContextGLM,
    /// Y14→Y30 bridge model — learns the relationship between 14-day
    /// incremental fans (early signal) and 30-day durable fans (North Star).
    /// When Y30 is not yet available, the bridge predicts Y30 from Y14 with
    /// honest uncertainty, which inflates the treatment-effect std for
    /// decisions based on Y14 alone.
    pub bridge: Y14Y30Bridge,
}

/// Treatment-aware prediction statistics — the result of querying both the
/// outcome model and the treatment-effect model in a single call.
///
/// When `use_treatment_effect` is true, the EFE scorer should use
/// `treatment_effect` as the expected fans and `treatment_std` as the
/// uncertainty. When false, it should use `expected_fans` and `predict_std`
/// from the outcome model.
///
/// # Y30 North Star
///
/// The brain prefers Y30 (30-day durable) treatment effects as the primary
/// decision signal when Y30 confidence is sufficient. When Y30 is not yet
/// available (the 30-day window hasn't elapsed), the brain falls back to Y14
/// (14-day incremental) with **inflated uncertainty** via the Y14→Y30 bridge
/// model. The `uses_y30` flag indicates which target drove the decision.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct TreatmentAwareStats {
    /// Expected fans from the outcome model (fallback).
    pub expected_fans: f64,
    /// Treatment effect τ(x) — Y30 if confident, Y14 with bridge inflation
    /// otherwise.
    pub treatment_effect: f64,
    /// Uncertainty in the treatment effect (inflated via bridge when using
    /// Y14 as a proxy for Y30).
    pub treatment_std: f64,
    /// Uncertainty in the outcome model prediction.
    pub predict_std: f64,
    /// Outcome model confidence (observation count).
    pub confidence: u32,
    /// Treatment-effect model confidence (Y14 paired observation count).
    pub treatment_confidence: u32,
    /// Whether to use the treatment effect as the primary signal.
    pub use_treatment_effect: bool,
    /// Y30 treatment effect τ_y30(x). Available when Y30 confidence is
    /// sufficient; otherwise 0.0.
    pub treatment_effect_y30: f64,
    /// Uncertainty in the Y30 treatment effect.
    pub treatment_std_y30: f64,
    /// Y30 treatment-effect model confidence (paired observation count).
    pub treatment_confidence_y30: u32,
    /// Whether the decision used Y30 (true) or Y14 with bridge inflation
    /// (false). When `use_treatment_effect` is false, this is also false.
    pub uses_y30: bool,
    /// P(τ > δ) — probability that the treatment effect exceeds the
    /// meaningful-effect threshold. Computed from the Y30 (or Y14-bridged)
    /// posterior. This is a more decision-relevant signal than just the
    /// mean treatment effect.
    pub p_meaningful_effect: f64,
}

/// A Bayesian linear regression bridge model: Y30 = α + β·Y14 + ε.
///
/// This model learns the relationship between the 14-day incremental fan
/// count (Y14, the early leading signal) and the 30-day durable fan count
/// (Y30, the North Star). When Y30 is not yet available (the 30-day window
/// hasn't elapsed), the bridge predicts Y30 from Y14 with honest uncertainty.
///
/// The bridge is updated from evidence rows that have BOTH Y14 and Y30
/// outcomes. Early in the system's life, no such rows exist (Y30 takes 30
/// days to arrive), so the bridge starts with a prior of β=1, α=0 (Y30 ≈ Y14)
/// and wide uncertainty.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Y14Y30Bridge {
    /// Posterior mean: [intercept α, slope β].
    mu: [f64; 2],
    /// Posterior covariance (2×2).
    sigma: [[f64; 2]; 2],
    /// Number of paired (Y14, Y30) observations.
    n: u32,
    /// Estimated residual variance σ². Updated via running mean of squared
    /// residuals.
    residual_variance: f64,
}

impl Default for Y14Y30Bridge {
    fn default() -> Self {
        Self::new()
    }
}

impl Y14Y30Bridge {
    /// Prior: α=0, β=1 (Y30 ≈ Y14), wide variance, residual variance = 4.0.
    #[must_use]
    pub fn new() -> Self {
        Self {
            mu: [0.0, 1.0],
            sigma: [[4.0, 0.0], [0.0, 4.0]],
            n: 0,
            residual_variance: 4.0,
        }
    }

    /// Predicts Y30 from Y14. Returns (mean, variance).
    ///
    /// The variance includes both the posterior uncertainty in (α, β) and
    /// the residual variance σ². This is the honest predictive uncertainty
    /// for Y30 given Y14.
    #[must_use]
    pub fn predict(&self, y14: f64) -> (f64, f64) {
        let x = [1.0, y14];
        let mean = x[0] * self.mu[0] + x[1] * self.mu[1];
        // Predictive variance: xᵀΣx + σ²_residual
        let var = x[0] * x[0] * self.sigma[0][0]
            + 2.0 * x[0] * x[1] * self.sigma[0][1]
            + x[1] * x[1] * self.sigma[1][1]
            + self.residual_variance;
        (mean, var.max(0.01))
    }

    /// Updates the bridge from a paired (Y14, Y30) observation.
    ///
    /// Uses online Bayesian linear regression with a 2×2 matrix inverse.
    pub fn update(&mut self, y14: f64, y30: f64) {
        if !y14.is_finite() || !y30.is_finite() {
            return;
        }
        let x = [1.0, y14];
        let sigma2 = self.residual_variance.max(0.5); // floor for stability

        // Prior precision: Σ⁻¹
        let det = self.sigma[0][0] * self.sigma[1][1] - self.sigma[0][1] * self.sigma[1][0];
        if det.abs() < 1e-12 {
            return;
        }
        let lambda = [
            [self.sigma[1][1] / det, -self.sigma[0][1] / det],
            [-self.sigma[1][0] / det, self.sigma[0][0] / det],
        ];

        // Hessian: H = Σ⁻¹ + xxᵀ/σ²
        let mut h = lambda;
        for i in 0..2 {
            for j in 0..2 {
                h[i][j] += x[i] * x[j] / sigma2;
            }
        }

        // Invert H (2×2)
        let h_det = h[0][0] * h[1][1] - h[0][1] * h[1][0];
        if h_det.abs() < 1e-12 {
            return;
        }
        let h_inv = [
            [h[1][1] / h_det, -h[0][1] / h_det],
            [-h[1][0] / h_det, h[0][0] / h_det],
        ];

        // Gradient: g = x·y/σ²
        let g = [x[0] * y30 / sigma2, x[1] * y30 / sigma2];

        // Prior contribution: Σ⁻¹·μ
        let prior_contrib = [
            lambda[0][0] * self.mu[0] + lambda[0][1] * self.mu[1],
            lambda[1][0] * self.mu[0] + lambda[1][1] * self.mu[1],
        ];

        // Posterior mean: μ_new = H⁻¹·(Σ⁻¹·μ + g)
        let total = [prior_contrib[0] + g[0], prior_contrib[1] + g[1]];
        self.mu[0] = h_inv[0][0] * total[0] + h_inv[0][1] * total[1];
        self.mu[1] = h_inv[1][0] * total[0] + h_inv[1][1] * total[1];
        self.sigma = h_inv;

        // Update residual variance via running mean of squared residuals.
        let predicted = x[0] * self.mu[0] + x[1] * self.mu[1];
        let resid = y30 - predicted;
        let alpha = 1.0 / (self.n as f64 + 1.0).max(1.0);
        self.residual_variance = (1.0 - alpha) * self.residual_variance + alpha * resid * resid;
        self.residual_variance = self.residual_variance.max(0.5); // floor

        self.n += 1;
    }

    /// Returns the number of paired observations.
    #[must_use]
    pub fn confidence(&self) -> u32 {
        self.n
    }

    /// Returns the slope β (how Y30 scales with Y14).
    #[must_use]
    pub fn slope(&self) -> f64 {
        self.mu[1]
    }
}

impl CausalModel {
    /// Creates a causal model with the default priors.
    #[must_use]
    pub fn new() -> Self {
        Self {
            fans: HierarchicalNegBinPosterior::new(NegBinPosterior::prior(
                DEFAULT_EXPECTED_FANS,
                1.0, // dispersion=1.0 → prior rate variance = 4.0, matching old Normal prior
            )),
            treatment_effects: TreatmentEffectPosterior::new(),
            treatment_effects_y30: TreatmentEffectPosterior::new(),
            template_expected_signal: HashMap::new(),
            calibration: crate::calibration::CalibrationTracker::new(),
            calibration_y30: crate::calibration::CalibrationTracker::new(),
            context_effects: ContextGLM::new(),
            bridge: Y14Y30Bridge::new(),
        }
    }

    /// Predicts expected new fans for a dispatch given its context.
    ///
    /// Combines the hierarchical NegBin posterior mean with a learned
    /// log-linear context GLM that modulates the base prediction based on
    /// event proximity, growth trend, and subreddit type.
    ///
    /// `post_format`, `time_of_day_bps`, and `community_novelty_bps` are
    /// carried by [`DispatchContext`] but not yet wired into the GLM —
    /// they are reserved for future context features so the shape is stable
    /// before the coefficients exist.
    #[must_use]
    pub fn predict(&self, template_id: &str, context: &DispatchContext) -> f64 {
        self.predict_stats(template_id, context).0
    }

    /// Predicts expected Signal installs for a dispatch. Uses the
    /// template-level Signal EMA if available, otherwise falls back to
    /// 10% of the fan prediction (a reasonable conversion prior). Context
    /// adjustments are applied via the same `ContextGLM` as fans.
    #[must_use]
    pub fn predict_signal(&self, template_id: &str, context: &DispatchContext) -> f64 {
        if let Some(&signal_prior) = self.template_expected_signal.get(template_id) {
            // Apply the same learned context GLM as fans — the context
            // effects (event proximity, growth trend) modulate Signal
            // installs the same way they modulate fan acquisition.
            return self.context_effects.predict(signal_prior, context).max(0.0);
        }
        // No learned prior: fall back to 10% of fan prediction.
        self.predict(template_id, context) * 0.1
    }

    /// Returns the prediction standard deviation for a template.
    /// Used by the EFE scorer to quantify epistemic uncertainty.
    /// Returns the posterior std dev from the hierarchical model.
    #[must_use]
    pub fn predict_std(&self, template_id: &str) -> f64 {
        // Use the pooled variance from predict() to avoid a separate
        // lookup + clone. The floor matches the previous behaviour.
        let (_mean, var) = self.fans.predict(template_id, None);
        var.sqrt().max(0.1)
    }

    /// Updates the model from a prediction outcome (the dopamine loop).
    ///
    /// Updates the fan count posterior (Gamma-Poisson conjugate), the
    /// learned context GLM, the per-template Signal EMA, and the Y14
    /// calibration tracker. The Y14/Y30 treatment-effect posteriors are
    /// NOT updated here — they are updated from evidence rows via
    /// [`CausalModel::update_treatment_effect`] /
    /// [`CausalModel::update_treatment_effect_y30`].
    pub fn update(&mut self, outcome: &PredictionOutcome) {
        let template = &outcome.prediction.template_id;
        let subreddit_type = outcome.prediction.context.subreddit_type.as_deref();
        // Get the base prediction (posterior mean before context adjustment)
        // so we can learn the context effect from the ratio.
        let (base_mean, _) = self.fans.predict(template, subreddit_type);
        // Update the hierarchical fan posterior (Gamma-Poisson conjugate).
        // Fan counts are non-negative integers — convert to u32.
        let observed_count = outcome.observed_new_fans.round().max(0.0) as u32;
        self.fans
            .update(Some(template), subreddit_type, observed_count);
        // Update the learned context effects from the implied multiplier.
        // base_mean is the raw posterior mean before context adjustment;
        // the ratio observed/base implies the context multiplier.
        self.context_effects.update(
            &outcome.prediction.context,
            base_mean,
            outcome.observed_new_fans,
        );
        // Update the Signal install EMA.
        let confidence = self.fans.confidence(template);
        let lr = 1.0 / (1.0 + (confidence as f64).min(10.0));
        let signal_current = self
            .template_expected_signal
            .get(template)
            .copied()
            .unwrap_or(DEFAULT_EXPECTED_SIGNAL);
        let signal_updated =
            signal_current + lr * (outcome.observed_signal_installs - signal_current);
        self.template_expected_signal
            .insert(template.clone(), signal_updated.max(0.0));
        // Record the prediction-observation pair for calibration. This
        // lets the brain detect and correct systematic prediction bias
        // (e.g. consistently over-predicting fan growth).
        self.calibration.record(
            template,
            outcome.prediction.expected_new_fans,
            self.predict_std(template),
            outcome.observed_new_fans,
        );
    }

    /// Returns the confidence (measurement count) for a template.
    #[must_use]
    pub fn confidence(&self, template_id: &str) -> u32 {
        self.fans.confidence(template_id)
    }

    /// Returns `(expected_fans, predict_std, confidence)` in a single
    /// hierarchical lookup. This is the hot path for EFE scoring — calling
    /// [`predict`](Self::predict), [`predict_std`](Self::predict_std), and
    /// [`confidence`](Self::confidence) separately does three HashMap
    /// lookups for the same template. This method does one.
    #[must_use]
    pub fn predict_stats(&self, template_id: &str, context: &DispatchContext) -> (f64, f64, u32) {
        let (mean, var) = self
            .fans
            .predict(template_id, context.subreddit_type.as_deref());
        let expected_fans = self.context_effects.predict(mean, context);
        let predict_std = var.sqrt().max(0.1);
        let confidence = self.fans.confidence(template_id);
        (expected_fans, predict_std, confidence)
    }

    /// Returns the expected fan count for a template, or the default prior.
    #[must_use]
    pub fn expected_fans(&self, template_id: &str) -> f64 {
        let post = self.fans.template_posterior(template_id);
        post.mean()
    }

    /// Returns P(expected_fans > 0) for a template — the probability that
    /// this template produces any fans at all.
    #[must_use]
    pub fn p_positive(&self, template_id: &str) -> f64 {
        let post = self.fans.template_posterior(template_id);
        post.p_positive()
    }

    /// Updates the treatment-effect posterior from an experiment outcome.
    ///
    /// `observed_tau` is the estimated treatment effect (e.g. from an IPW
    /// estimator). It can be negative. `observation_variance` is the variance
    /// of the estimate.
    pub fn update_treatment_effect(
        &mut self,
        template_id: &str,
        subreddit_type: Option<&str>,
        observed_tau: f64,
        observation_variance: f64,
    ) {
        self.treatment_effects.update(
            template_id,
            subreddit_type,
            observed_tau,
            observation_variance,
        );
    }

    /// Updates the Y30 (30-day durable) treatment-effect posterior.
    ///
    /// This is the North Star target — durable fans still active after 30
    /// days. The Y30 outcome arrives later than Y14, so this posterior
    /// accumulates data more slowly. When Y30 confidence is low, the
    /// decision path falls back to Y14 with bridge-inflated uncertainty.
    pub fn update_treatment_effect_y30(
        &mut self,
        template_id: &str,
        subreddit_type: Option<&str>,
        observed_tau_y30: f64,
        observation_variance: f64,
    ) {
        self.treatment_effects_y30.update(
            template_id,
            subreddit_type,
            observed_tau_y30,
            observation_variance,
        );
    }

    /// Updates the Y14→Y30 bridge model from a paired observation.
    ///
    /// Both Y14 and Y30 must be available. The bridge learns the relationship
    /// E[Y30 | Y14] so that when only Y14 is available, the brain can predict
    /// Y30 with honest uncertainty.
    pub fn update_bridge(&mut self, y14: f64, y30: f64) {
        self.bridge.update(y14, y30);
    }

    /// Returns treatment-aware prediction statistics — both the outcome model
    /// and the treatment-effect model in a single call.
    ///
    /// # Y30-First Decision Logic
    ///
    /// The brain prefers Y30 (30-day durable) treatment effects as the
    /// primary decision signal:
    ///
    /// 1. **Y30 confident** (`treatment_confidence_y30 >= MIN_TREATMENT_CONFIDENCE`):
    ///    Use the Y30 treatment effect directly. `uses_y30 = true`.
    /// 2. **Y14 confident but Y30 not yet** (`treatment_confidence >= MIN_TREATMENT_CONFIDENCE`):
    ///    Use the Y14 treatment effect but **inflate the uncertainty** via
    ///    the Y14→Y30 bridge model. The bridge predicts Y30 from Y14 and
    ///    adds the bridge's predictive variance to the treatment std.
    ///    `uses_y30 = false`.
    /// 3. **Neither confident**: Fall back to the outcome model.
    ///    `use_treatment_effect = false`.
    ///
    /// This ensures the brain doesn't over-optimize for the short-term Y14
    /// signal at the expense of the long-term Y30 North Star.
    #[must_use]
    pub fn predict_stats_with_treatment(
        &self,
        template_id: &str,
        context: &DispatchContext,
    ) -> TreatmentAwareStats {
        let (expected_fans, predict_std, confidence) = self.predict_stats(template_id, context);
        // Apply calibration correction to the outcome model prediction.
        let expected_fans = self.calibration.correct_prediction(expected_fans).max(0.0);

        // Y14 treatment effect (early leading signal).
        let (tau_y14, std_y14, conf_y14) = self
            .treatment_effects
            .predict_stats(template_id, context.subreddit_type.as_deref());

        // Y30 treatment effect (North Star — arrives later).
        let (tau_y30, std_y30, conf_y30) = self
            .treatment_effects_y30
            .predict_stats(template_id, context.subreddit_type.as_deref());

        // Y30-first decision logic with hysteresis.
        //
        // Hysteresis: the switch threshold depends on the current state.
        // If we're already using the treatment effect, we require confidence
        // to drop below `MIN_TREATMENT_CONFIDENCE - HYSTERESIS_BAND` before
        // switching back. This prevents oscillation near the threshold.
        // Since this is a stateless function, we approximate hysteresis by
        // using a lower threshold for the fallback path: once either Y14 or
        // Y30 crosses MIN_TREATMENT_CONFIDENCE, the brain needs to drop
        // significantly below it to fall back.
        let y30_confident = conf_y30 >= MIN_TREATMENT_CONFIDENCE;
        let y14_confident = conf_y14 >= MIN_TREATMENT_CONFIDENCE;
        // Hysteresis fallback threshold: if both are below this, fall back.
        let hysteresis_floor = MIN_TREATMENT_CONFIDENCE.saturating_sub(HYSTERESIS_BAND);

        let (treatment_effect, treatment_std, treatment_confidence, use_treatment, uses_y30) =
            if y30_confident {
                // Y30 is confident — use it directly as the North Star.
                (tau_y30, std_y30, conf_y30, true, true)
            } else if y14_confident {
                // Y14 is confident but Y30 isn't yet. Use Y14 as a proxy
                // for Y30, but inflate the uncertainty via the bridge model.
                let (_, bridge_var) = self.bridge.predict(tau_y14);
                let inflated_std = (std_y14 * std_y14 + bridge_var).sqrt();
                (tau_y14, inflated_std, conf_y14, true, false)
            } else if conf_y30 >= hysteresis_floor || conf_y14 >= hysteresis_floor {
                // Hysteresis: one of the posteriors is in the hysteresis band
                // (between hysteresis_floor and MIN_TREATMENT_CONFIDENCE).
                // Keep using the treatment effect to avoid oscillation.
                if conf_y30 >= conf_y14 {
                    (tau_y30, std_y30, conf_y30, true, true)
                } else {
                    let (_, bridge_var) = self.bridge.predict(tau_y14);
                    let inflated_std = (std_y14 * std_y14 + bridge_var).sqrt();
                    (tau_y14, inflated_std, conf_y14, true, false)
                }
            } else {
                // Both are below the hysteresis floor — fall back to outcome.
                (tau_y14, std_y14, conf_y14, false, false)
            };

        // P(τ > δ) — meaningful-effect probability.
        // Computed from whichever treatment-effect posterior is being used.
        let p_meaningful = if uses_y30 {
            self.treatment_effects_y30.p_meaningful_effect(
                template_id,
                context.subreddit_type.as_deref(),
                MEANINGFUL_EFFECT_THRESHOLD,
            )
        } else {
            self.treatment_effects.p_meaningful_effect(
                template_id,
                context.subreddit_type.as_deref(),
                MEANINGFUL_EFFECT_THRESHOLD,
            )
        };

        TreatmentAwareStats {
            expected_fans,
            treatment_effect,
            treatment_std,
            predict_std,
            confidence,
            treatment_confidence,
            use_treatment_effect: use_treatment,
            treatment_effect_y30: tau_y30,
            treatment_std_y30: std_y30,
            treatment_confidence_y30: conf_y30,
            uses_y30,
            p_meaningful_effect: p_meaningful,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn causal_model_uses_default_prior_for_unknown_template() {
        let model = CausalModel::new();
        let ctx = DispatchContext::default();
        assert_eq!(
            model.predict("unknown-template", &ctx),
            DEFAULT_EXPECTED_FANS
        );
        assert_eq!(model.confidence("unknown-template"), 0);
    }

    #[test]
    fn causal_model_event_proximity_boosts_prediction() {
        let model = CausalModel::new();
        let ctx_close = DispatchContext {
            days_to_event: Some(5),
            ..Default::default()
        };
        let ctx_far = DispatchContext {
            days_to_event: Some(60),
            ..Default::default()
        };
        // Close event: 2.0 * 1.5 = 3.0
        assert!((model.predict("t", &ctx_close) - 3.0).abs() < 0.01);
        // Far event: no boost
        assert!((model.predict("t", &ctx_far) - 2.0).abs() < 0.01);
    }

    #[test]
    fn causal_model_stagnant_trend_reduces_prediction() {
        let model = CausalModel::new();
        let ctx = DispatchContext {
            fan_growth_trend: GrowthTrend::Stagnant,
            ..Default::default()
        };
        // Stagnant: 2.0 * 0.8 = 1.6
        assert!((model.predict("t", &ctx) - 1.6).abs() < 0.01);
    }

    #[test]
    fn causal_model_updates_from_prediction_error() {
        let mut model = CausalModel::new();
        let prediction = DispatchPrediction {
            template_id: "t".to_owned(),
            expected_new_fans: 2.0,
            ..Default::default()
        };
        // Observed 5 fans — better than expected.
        let outcome = PredictionOutcome::from_observation(prediction, 5.0, 0.0);
        model.update(&outcome);
        // After one Bayesian update, the mean should move toward 5.
        let updated = model.expected_fans("t");
        assert!(
            updated > 2.0 && updated < 5.0,
            "mean should move toward observation, got {updated}"
        );
        assert_eq!(model.confidence("t"), 1);
    }

    #[test]
    fn causal_model_learning_rate_decays_with_confidence() {
        let mut model = CausalModel::new();
        // First update: moves significantly toward observed.
        let p1 = DispatchPrediction {
            template_id: "t".to_owned(),
            expected_new_fans: 2.0,
            ..Default::default()
        };
        model.update(&PredictionOutcome::from_observation(p1, 10.0, 0.0));
        let after_1 = model.expected_fans("t");
        // After many updates with observed=0, the model moves toward 0
        // but each step is smaller (Bayesian precision grows).
        for _ in 0..20 {
            let p = DispatchPrediction {
                template_id: "t".to_owned(),
                expected_new_fans: 10.0,
                ..Default::default()
            };
            model.update(&PredictionOutcome::from_observation(p, 0.0, 0.0));
        }
        let final_val = model.expected_fans("t");
        assert!(final_val < after_1, "model should have moved toward 0");
        assert!(final_val > 0.0, "but never reaches exactly 0");
        assert_eq!(model.confidence("t"), 21);
    }

    #[test]
    fn prediction_error_computes_dopamine_signal() {
        let prediction = DispatchPrediction {
            template_id: "t".to_owned(),
            expected_new_fans: 3.0,
            expected_signal_installs: 1.0,
            ..Default::default()
        };
        let outcome = PredictionOutcome::from_observation(prediction, 7.0, 0.5);
        // Positive fan error: better than expected.
        assert!((outcome.fan_prediction_error - 4.0).abs() < 0.01);
        // Negative signal error: worse than expected.
        assert!((outcome.signal_prediction_error - (-0.5)).abs() < 0.01);
    }

    #[test]
    fn causal_model_variance_starts_at_prior() {
        let model = CausalModel::new();
        // Unmeasured template: the NegBin prior has mean=2.0, dispersion=1.0.
        // α=1.0, β=0.5. Rate variance = α/β² = 1.0/0.25 = 4.0. std = 2.0.
        let std = model.predict_std("t");
        assert!(
            (std - 2.0).abs() < 0.01,
            "prior std should be 2.0, got {std}"
        );
    }

    #[test]
    fn causal_model_variance_shrinks_with_consistent_observations() {
        let mut model = CausalModel::new();
        for _ in 0..10 {
            let p = DispatchPrediction {
                template_id: "t".to_owned(),
                expected_new_fans: 5.0,
                ..Default::default()
            };
            model.update(&PredictionOutcome::from_observation(p, 5.0, 0.0));
        }
        let std = model.predict_std("t");
        assert!(
            std < 1.0,
            "variance should shrink with consistent observations, got std={std}"
        );
    }

    #[test]
    fn causal_model_variance_grows_with_variable_observations() {
        let mut model = CausalModel::new();
        for i in 0..10 {
            let observed = if i % 2 == 0 { 10.0 } else { 0.0 };
            let p = DispatchPrediction {
                template_id: "t".to_owned(),
                expected_new_fans: 5.0,
                ..Default::default()
            };
            model.update(&PredictionOutcome::from_observation(p, observed, 0.0));
        }
        let std = model.predict_std("t");
        assert!(
            std > 0.3,
            "variance should reflect variable observations, got std={std}"
        );
    }

    #[test]
    fn causal_model_signal_prediction_defaults_to_10_percent() {
        let model = CausalModel::new();
        let ctx = DispatchContext::default();
        let signal = model.predict_signal("t", &ctx);
        let fans = model.predict("t", &ctx);
        assert!((signal - fans * 0.1).abs() < 0.01);
    }

    #[test]
    fn causal_model_signal_prediction_learns_independently() {
        let mut model = CausalModel::new();
        let p = DispatchPrediction {
            template_id: "t".to_owned(),
            expected_new_fans: 5.0,
            expected_signal_installs: 0.5,
            ..Default::default()
        };
        model.update(&PredictionOutcome::from_observation(p, 5.0, 3.0));
        let learned = model
            .template_expected_signal
            .get("t")
            .copied()
            .unwrap_or(0.0);
        assert!(
            learned > 1.0,
            "Signal prediction should learn from observed installs, got {learned}"
        );
    }

    #[test]
    fn causal_model_subreddit_type_adjusts_prediction() {
        let mut model = CausalModel::new();
        for _ in 0..10 {
            let p = DispatchPrediction {
                template_id: "t".to_owned(),
                expected_new_fans: 2.0,
                context: DispatchContext {
                    subreddit_type: Some("metal".to_owned()),
                    ..Default::default()
                },
                ..Default::default()
            };
            model.update(&PredictionOutcome::from_observation(p, 8.0, 0.0));
        }
        let ctx_metal = DispatchContext {
            subreddit_type: Some("metal".to_owned()),
            ..Default::default()
        };
        let ctx_none = DispatchContext::default();
        let pred_metal = model.predict("t", &ctx_metal);
        let pred_none = model.predict("t", &ctx_none);
        assert!(
            pred_metal >= pred_none,
            "learned subreddit type should boost prediction: metal={pred_metal}, none={pred_none}"
        );
    }

    #[test]
    fn p_positive_for_effective_template() {
        let mut model = CausalModel::new();
        for _ in 0..10 {
            let p = DispatchPrediction {
                template_id: "t".to_owned(),
                expected_new_fans: 2.0,
                ..Default::default()
            };
            model.update(&PredictionOutcome::from_observation(p, 10.0, 0.0));
        }
        // After 10 observations of 10.0, P(>0) should be very high.
        assert!(
            model.p_positive("t") > 0.99,
            "effective template should have P(>0) ≈ 1"
        );
    }

    #[test]
    fn treatment_effect_falls_back_when_no_data() {
        let model = CausalModel::new();
        let ctx = DispatchContext::default();
        let stats = model.predict_stats_with_treatment("t", &ctx);
        assert!(!stats.use_treatment_effect);
        assert_eq!(stats.treatment_confidence, 0);
        assert!((stats.treatment_effect - 0.0).abs() < 1e-9);
    }

    #[test]
    fn treatment_effect_falls_back_below_threshold() {
        let mut model = CausalModel::new();
        // Only 2 observations — below the hysteresis floor (5-2=3).
        for _ in 0..2 {
            model.update_treatment_effect("t", None, 5.0, 1.0);
        }
        let ctx = DispatchContext::default();
        let stats = model.predict_stats_with_treatment("t", &ctx);
        assert!(!stats.use_treatment_effect);
        assert_eq!(stats.treatment_confidence, 2);
    }

    #[test]
    fn treatment_effect_used_above_threshold() {
        let mut model = CausalModel::new();
        // 10 observations — above MIN_TREATMENT_CONFIDENCE (5).
        for _ in 0..10 {
            model.update_treatment_effect("t", None, 5.0, 1.0);
        }
        let ctx = DispatchContext::default();
        let stats = model.predict_stats_with_treatment("t", &ctx);
        assert!(stats.use_treatment_effect);
        assert_eq!(stats.treatment_confidence, 10);
        assert!(stats.treatment_effect > 2.0);
    }

    #[test]
    fn treatment_effect_can_be_negative() {
        let mut model = CausalModel::new();
        for _ in 0..10 {
            model.update_treatment_effect("bad", None, -3.0, 1.0);
        }
        let ctx = DispatchContext::default();
        let stats = model.predict_stats_with_treatment("bad", &ctx);
        assert!(stats.use_treatment_effect);
        assert!(stats.treatment_effect < -1.0);
    }

    #[test]
    fn treatment_effect_respects_subreddit_type() {
        let mut model = CausalModel::new();
        // Observe high values for "metal" subreddit type.
        for _ in 0..10 {
            model.update_treatment_effect("t", Some("metal"), 8.0, 1.0);
        }
        // Observe low values for "pop" subreddit type to move the global down.
        for _ in 0..10 {
            model.update_treatment_effect("t", Some("pop"), 0.0, 1.0);
        }
        let ctx_metal = DispatchContext {
            subreddit_type: Some("metal".to_owned()),
            ..Default::default()
        };
        let ctx_none = DispatchContext::default();
        let stats_metal = model.predict_stats_with_treatment("t", &ctx_metal);
        let stats_none = model.predict_stats_with_treatment("t", &ctx_none);
        assert!(
            stats_metal.treatment_effect > stats_none.treatment_effect,
            "metal subreddit type should boost treatment effect above global, got metal={} none={}",
            stats_metal.treatment_effect,
            stats_none.treatment_effect
        );
    }

    // ── Y30 North Star + Y14 bridge tests (P0.3) ─────────────────────────

    #[test]
    fn y30_preferred_when_confident() {
        let mut model = CausalModel::new();
        // Build Y30 confidence above MIN_TREATMENT_CONFIDENCE.
        for _ in 0..10 {
            model.update_treatment_effect_y30("t", None, 5.0, 1.0);
        }
        // Also build Y14 confidence (but Y30 should be preferred).
        for _ in 0..10 {
            model.update_treatment_effect("t", None, 3.0, 1.0);
        }
        let ctx = DispatchContext::default();
        let stats = model.predict_stats_with_treatment("t", &ctx);
        assert!(
            stats.uses_y30,
            "should use Y30 when Y30 confidence >= threshold"
        );
        assert!(
            stats.use_treatment_effect,
            "should use treatment effect (Y30 is confident)"
        );
        // The treatment effect should be the Y30 value (5.0), not Y14 (3.0).
        assert!(
            (stats.treatment_effect - 5.0).abs() < 2.0,
            "treatment effect should be close to Y30 (5.0), got {}",
            stats.treatment_effect
        );
    }

    #[test]
    fn y14_bridge_inflates_uncertainty() {
        let mut model = CausalModel::new();
        // Build Y14 confidence but NOT Y30 confidence.
        for _ in 0..10 {
            model.update_treatment_effect("t", None, 5.0, 1.0);
        }
        let ctx = DispatchContext::default();
        let stats = model.predict_stats_with_treatment("t", &ctx);
        assert!(
            stats.use_treatment_effect,
            "should use Y14 treatment effect (confident)"
        );
        assert!(!stats.uses_y30, "should not use Y30 (not confident yet)");
        // The treatment_std should be inflated by the bridge.
        // Compare against the raw Y14 std.
        let (_raw_tau, raw_std, _) = model.treatment_effects.predict_stats("t", None);
        assert!(
            stats.treatment_std > raw_std,
            "bridge should inflate uncertainty, got stats_std={:.4} raw_std={:.4}",
            stats.treatment_std,
            raw_std
        );
    }

    #[test]
    fn bridge_learns_y14_y30_relationship() {
        let mut model = CausalModel::new();
        // True relationship: Y30 = 0.5 * Y14 (durable fans are half of incremental).
        // Update the bridge with paired observations.
        for y14 in [2.0, 4.0, 6.0, 8.0, 10.0, 12.0, 14.0, 16.0] {
            let y30 = 0.5 * y14;
            model.update_bridge(y14, y30);
        }
        // The bridge slope should have moved toward 0.5.
        let slope = model.bridge.slope();
        assert!(
            (slope - 0.5).abs() < 0.3,
            "bridge slope should converge toward 0.5, got {slope:.3}"
        );
        // Predict Y30 from Y14=10 → should be ~5.0.
        let (predicted, _) = model.bridge.predict(10.0);
        assert!(
            (predicted - 5.0).abs() < 2.0,
            "bridge should predict Y30≈5.0 from Y14=10, got {predicted:.3}"
        );
    }

    #[test]
    fn y30_falls_back_to_outcome_when_neither_confident() {
        let model = CausalModel::new();
        let ctx = DispatchContext::default();
        let stats = model.predict_stats_with_treatment("t", &ctx);
        assert!(
            !stats.use_treatment_effect,
            "should fall back to outcome model when neither Y14 nor Y30 is confident"
        );
        assert!(!stats.uses_y30);
    }

    // ── P(τ > δ) meaningful-effect tests (P1.6) ──────────────────────────

    #[test]
    fn p_meaningful_effect_is_high_for_strong_template() {
        let mut model = CausalModel::new();
        // Build Y14 confidence with strong positive effects.
        for _ in 0..20 {
            model.update_treatment_effect("t", None, 10.0, 1.0);
        }
        let ctx = DispatchContext::default();
        let stats = model.predict_stats_with_treatment("t", &ctx);
        // P(τ > 1.0) should be high — the mean is ~10 and std is small.
        assert!(
            stats.p_meaningful_effect > 0.9,
            "P(τ > δ) should be high for strong template, got {}",
            stats.p_meaningful_effect
        );
    }

    #[test]
    fn p_meaningful_effect_is_low_for_negative_template() {
        let mut model = CausalModel::new();
        for _ in 0..20 {
            model.update_treatment_effect("bad", None, -5.0, 1.0);
        }
        let ctx = DispatchContext::default();
        let stats = model.predict_stats_with_treatment("bad", &ctx);
        // P(τ > 1.0) should be very low — the mean is ~-5.
        assert!(
            stats.p_meaningful_effect < 0.01,
            "P(τ > δ) should be low for negative template, got {}",
            stats.p_meaningful_effect
        );
    }

    #[test]
    fn p_meaningful_effect_is_moderate_for_uncertain_template() {
        let mut model = CausalModel::new();
        // Mean near the threshold, high variance.
        for _ in 0..5 {
            model.update_treatment_effect("t", None, 2.0, 5.0);
        }
        let ctx = DispatchContext::default();
        let stats = model.predict_stats_with_treatment("t", &ctx);
        // P(τ > 1.0) should be somewhere in the middle — not extreme.
        assert!(
            stats.p_meaningful_effect > 0.3 && stats.p_meaningful_effect < 0.9,
            "P(τ > δ) should be moderate for uncertain template, got {}",
            stats.p_meaningful_effect
        );
    }

    // ── Hysteresis tests (P1.7) ──────────────────────────────────────────

    #[test]
    fn hysteresis_keeps_treatment_effect_in_band() {
        let mut model = CausalModel::new();
        // Build Y14 confidence to exactly MIN_TREATMENT_CONFIDENCE (5).
        for _ in 0..5 {
            model.update_treatment_effect("t", None, 5.0, 1.0);
        }
        let ctx = DispatchContext::default();
        let stats = model.predict_stats_with_treatment("t", &ctx);
        assert!(
            stats.use_treatment_effect,
            "should use treatment effect at MIN_TREATMENT_CONFIDENCE"
        );
        // Now add one more observation with a low effect — this should
        // increase confidence to 6 but lower the mean. The hysteresis
        // should keep us in the treatment-effect mode.
        model.update_treatment_effect("t", None, 0.0, 1.0);
        let stats2 = model.predict_stats_with_treatment("t", &ctx);
        assert!(
            stats2.use_treatment_effect,
            "hysteresis should keep treatment effect active in the band"
        );
    }

    // ── Checkpoint compatibility ──────────────────────────────────────────
    //
    // Old checkpoints in `viryaos_brain_state` may carry fields that were
    // removed from `CausalModel` (reach_model, funnel, rich_state_transitions).
    // Serde ignores unknown fields, so deserialization must keep working —
    // a shape change here is exactly the failure class that produced the
    // autopilot control-overview 500.

    #[test]
    fn deserializes_legacy_checkpoint_with_removed_fields() {
        let mut json = serde_json::to_value(CausalModel::new()).unwrap();
        let obj = json.as_object_mut().unwrap();
        obj.insert(
            "reach_model".to_owned(),
            serde_json::json!({ "conversions": {}, "exposures": {} }),
        );
        obj.insert("funnel".to_owned(), serde_json::json!({}));
        obj.insert("rich_state_transitions".to_owned(), serde_json::json!({}));

        let model: CausalModel = serde_json::from_value(json)
            .expect("legacy checkpoint with removed fields must still deserialize");
        let ctx = DispatchContext::default();
        assert!(model.predict("t", &ctx) > 0.0);
    }
}
