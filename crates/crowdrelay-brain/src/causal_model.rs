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

use crate::bayesian::{HierarchicalNegBinPosterior, NegBinPosterior};
use crate::bridge::Y14Y30Bridge;
use crate::context_effect::ContextGLM;
use crate::treatment_effect::TreatmentEffectPosterior;
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
    /// The specific target this dispatch was aimed at, e.g.
    /// `community:<target_id>`. `None` for workspace-wide templates, which
    /// genuinely have no target.
    ///
    /// This is deliberately outside [`DispatchContext`]: the context hash is
    /// the exploration and evidence key, and putting a per-target value in it
    /// would make every target its own context and every context novel
    /// forever. The target is a level of the causal hierarchy, not a context
    /// feature.
    #[serde(default)]
    pub target_key: Option<String>,
    /// The angle the post was asked to take. `None` for dispatches that have
    /// no creative surface (scanners, the strategist) and for rows written
    /// before families existed.
    ///
    /// Recorded, not yet learned from: see
    /// [`crowdrelay_domain::creative::CreativeFamily`] for why the estimator
    /// waits for data rather than the other way round.
    #[serde(default)]
    pub creative_family: Option<crowdrelay_domain::creative::CreativeFamily>,
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

/// The minimum number of paired (Y14, Y30) observations before the Y14→Y30
/// bridge model is considered reliable. Below this, the bridge's predictive
/// variance is inflated (up to 3× at 0 observations) to reflect that the
/// bridge is still a guess, not a calibrated model.
pub const MIN_BRIDGE_CONFIDENCE: u32 = 10;

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
    /// Regime-isolated calibration — separate trackers per estimation
    /// regime (Y30Direct, Y14Bridged, OutcomeModel). This ensures that
    /// a badly calibrated observational predictor cannot distort
    /// uncertainty for the randomized treatment estimator.
    pub calibration: crate::calibration::CalibrationByRegime,
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
    /// The Y14→Y30 bridge confidence (number of paired observations).
    /// 0 = the bridge is a pure prior (Y30 ≈ Y14 guess). 10+ = the bridge
    /// has been calibrated from real paired data.
    pub bridge_confidence: u32,
    /// Whether the bridge is reliable enough to trust Y14-bridged Y30
    /// estimates. True when `bridge_confidence >= MIN_BRIDGE_CONFIDENCE`.
    /// When false, the portfolio optimizer should weight Y14-bridged
    /// candidates lower — the bridge is a temporary belief, not a
    /// semi-factual substitute for Y30.
    pub bridge_is_reliable: bool,
    /// The strongest evidence quality available for this template/context.
    /// Propagated to DecisionValue for provenance. Phase 1: defaults to
    /// `Observational` since the causal model doesn't yet track per-template
    /// evidence quality. Phase 2 will derive this from the evidence table.
    pub evidence_quality: crate::evidence::EvidenceQuality,
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
            calibration: crate::calibration::CalibrationByRegime::new(),
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
        let target_key = outcome.prediction.target_key.as_deref();
        // Get the base prediction (posterior mean before context adjustment)
        // so we can learn the context effect from the ratio.
        let (base_mean, _) = self
            .fans
            .predict_for_target(template, subreddit_type, target_key);
        // Update the hierarchical fan posterior (Gamma-Poisson conjugate).
        // Fan counts are non-negative integers — convert to u32.
        let observed_count = outcome.observed_new_fans.round().max(0.0) as u32;
        self.fans
            .update_with_target(Some(template), subreddit_type, target_key, observed_count);
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
        // (e.g. consistently over-predicting fan growth). Routed to the
        // OutcomeModel regime tracker — the outcome model is the
        // observational predictor, separate from treatment-effect
        // calibration.
        self.calibration.record_by_regime(
            crate::decision_value::EstimationRegime::OutcomeModel,
            template,
            outcome.prediction.expected_new_fans,
            self.predict_std(template),
            outcome.observed_new_fans,
            outcome.prediction.context.subreddit_type.as_deref(),
            None,
            "observational",
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
        self.predict_stats_for_target(template_id, None, context)
    }

    /// Returns `(expected_fans, predict_std, confidence)` for one specific
    /// target. `target_key = None` reproduces [`predict_stats`] exactly, so
    /// workspace-wide templates are unaffected.
    #[must_use]
    pub fn predict_stats_for_target(
        &self,
        template_id: &str,
        target_key: Option<&str>,
        context: &DispatchContext,
    ) -> (f64, f64, u32) {
        let (mean, var) = self.fans.predict_for_target(
            template_id,
            context.subreddit_type.as_deref(),
            target_key,
        );
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
        self.update_treatment_effect_for_target(
            template_id,
            subreddit_type,
            None,
            observed_tau,
            observation_variance,
            crate::evidence::EvidenceQuality::Observational,
        );
    }

    /// Updates the Y14 treatment-effect posterior, attributing the
    /// observation to a specific target as well as the template and
    /// audience type.
    pub fn update_treatment_effect_for_target(
        &mut self,
        template_id: &str,
        subreddit_type: Option<&str>,
        target_key: Option<&str>,
        observed_tau: f64,
        observation_variance: f64,
        quality: crate::evidence::EvidenceQuality,
    ) {
        self.treatment_effects.update_with_quality(
            template_id,
            subreddit_type,
            target_key,
            observed_tau,
            observation_variance,
            quality,
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
        self.update_treatment_effect_y30_for_target(
            template_id,
            subreddit_type,
            None,
            observed_tau_y30,
            observation_variance,
            crate::evidence::EvidenceQuality::Observational,
        );
    }

    /// Updates the Y30 (durable) treatment-effect posterior for a specific
    /// target. This is the North Star signal at the level that decides which
    /// community is worth posting to.
    pub fn update_treatment_effect_y30_for_target(
        &mut self,
        template_id: &str,
        subreddit_type: Option<&str>,
        target_key: Option<&str>,
        observed_tau_y30: f64,
        observation_variance: f64,
        quality: crate::evidence::EvidenceQuality,
    ) {
        self.treatment_effects_y30.update_with_quality(
            template_id,
            subreddit_type,
            target_key,
            observed_tau_y30,
            observation_variance,
            quality,
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
        self.predict_stats_with_treatment_for_target(template_id, None, context)
    }

    /// Treatment-aware statistics for one specific target.
    ///
    /// Identical to [`predict_stats_with_treatment`] except that every
    /// hierarchical lookup includes the per-target level, so two communities
    /// in the same genre bucket no longer predict the same number. Passing
    /// `target_key = None` reproduces the template-level behaviour exactly.
    ///
    /// The regime switch (treatment effect versus outcome model) is still
    /// decided by template-level confidence — see
    /// [`crate::bayesian::TreatmentEffectPosterior::predict_stats_for_target`].
    #[must_use]
    pub fn predict_stats_with_treatment_for_target(
        &self,
        template_id: &str,
        target_key: Option<&str>,
        context: &DispatchContext,
    ) -> TreatmentAwareStats {
        let (expected_fans, predict_std, confidence) =
            self.predict_stats_for_target(template_id, target_key, context);
        // Apply regime-isolated calibration correction to the outcome
        // model prediction. The OutcomeModel regime tracker is separate
        // from Y30Direct/Y14Bridged — a bad observational calibration
        // cannot distort treatment-effect uncertainty.
        let expected_fans = self
            .calibration
            .correct_prediction_by_regime(
                crate::decision_value::EstimationRegime::OutcomeModel,
                expected_fans,
            )
            .max(0.0);

        // Y14 treatment effect (early leading signal).
        let (tau_y14, std_y14, conf_y14) = self.treatment_effects.predict_stats_for_target(
            template_id,
            context.subreddit_type.as_deref(),
            target_key,
        );

        // Y30 treatment effect (North Star — arrives later).
        let (tau_y30, std_y30, conf_y30) = self.treatment_effects_y30.predict_stats_for_target(
            template_id,
            context.subreddit_type.as_deref(),
            target_key,
        );

        // Y30-first decision logic.
        //
        // `MIN_TREATMENT_CONFIDENCE` is the whole threshold, in both directions.
        //
        // There used to be a third branch that accepted three or four
        // observations "to avoid oscillation". Hysteresis needs to know which
        // regime you were in last; this function knows only the posteriors, so
        // the branch was not hysteresis but a second, lower threshold that a
        // model reached first and never left. A fresh model with four
        // observations ranked on a treatment effect the stated minimum says it
        // must not trust yet. `GrowthStrategy::from_world_model_with_hysteresis`
        // shows what the real thing looks like: it takes the previous state as
        // an argument.
        let y30_confident = conf_y30 >= MIN_TREATMENT_CONFIDENCE;
        let y14_confident = conf_y14 >= MIN_TREATMENT_CONFIDENCE;

        let (treatment_effect, treatment_std, treatment_confidence, use_treatment, uses_y30) =
            if y30_confident {
                // Y30 is confident — use it directly as the North Star.
                (tau_y30, std_y30, conf_y30, true, true)
            } else if y14_confident {
                // Y14 is confident but Y30 isn't yet. The decision quantity is
                // still Y30, so the Y14 effect is carried across the bridge
                // rather than used as if it were already a Y30 number, and the
                // bridge's own uncertainty is added to the Y14 posterior's.
                //
                // The bridge is fitted on pairs of *incremental* outcomes —
                // both `observed_incremental_fans` and `durable_fans_30d` are
                // stored counterfactual-adjusted — so it already regresses one
                // effect on another and its prediction is the Y30 effect. That
                // is why the intercept is kept: with effects on both axes it
                // carries the durability a unit shows at zero fourteen-day
                // effect, which is a finding, not the level artefact an
                // intercept would be in a regression of raw counts.
                let (bridged_tau, bridge_var) = self.bridge.predict(tau_y14);
                let inflated_std = (std_y14 * std_y14 + bridge_var).sqrt();
                (bridged_tau, inflated_std, conf_y14, true, false)
            } else {
                // Neither horizon is identified yet — rank on the outcome model.
                (tau_y14, std_y14, conf_y14, false, false)
            };

        // P(τ > δ) — meaningful-effect probability, for the quantity actually
        // being ranked on.
        let p_meaningful = if uses_y30 {
            self.treatment_effects_y30.p_meaningful_effect(
                template_id,
                context.subreddit_type.as_deref(),
                MEANINGFUL_EFFECT_THRESHOLD,
            )
        } else if use_treatment {
            // Bridged regime. Reading the Y14 posterior here answered a
            // question about a different variable: it reported P(τ_y14 > δ)
            // while the decision was being made on the bridged Y30 effect,
            // with none of the bridge's uncertainty in it. The bridged
            // posterior is Normal by construction — a linear map of a Normal
            // plus independent bridge noise — so the probability is exact from
            // the mean and standard deviation already computed above.
            let z = (MEANINGFUL_EFFECT_THRESHOLD - treatment_effect) / treatment_std.max(0.1);
            1.0 - crate::bayesian::normal_cdf(z)
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
            bridge_confidence: self.bridge.confidence(),
            bridge_is_reliable: self.bridge.confidence() >= MIN_BRIDGE_CONFIDENCE,
            // Default to Observational — the application layer overrides
            // this via load_evidence_quality() when experiment assignments
            // exist. This is a conservative fallback, not a fake: when no
            // experiments have been run, Observational is the honest quality.
            evidence_quality: crate::evidence::EvidenceQuality::Observational,
        }
    }
}

#[cfg(test)]
mod tests;
