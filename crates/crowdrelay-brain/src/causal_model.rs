//! Causal Model — P(incremental_fan | template, context).
//!
//! The brain's causal model predicts how many incremental fans a worker
//! dispatch will produce, given the template and context features. It uses
//! proper Bayesian posteriors (Normal-Normal conjugate model) instead of
//! the old EMA + pseudo-variance.
//!
//! # Architecture
//!
//! - **Hierarchical posterior**: global + per-template + per-subreddit-type
//!   partial pooling. Low-confidence templates shrink toward the global mean.
//! - **Context adjustments**: event proximity and growth trend modulate the
//!   base prediction multiplicatively.
//! - **Independent Signal install learning**: Signal adoption has different
//!   drivers than fan acquisition, so it's learned separately.
//! - **Proper variance**: uses `NormalPosterior` which gives mathematically
//!   honest posterior variance, credible intervals, and P(positive).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::bayesian::{HierarchicalPosterior, NormalPosterior, TreatmentEffectPosterior};
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

/// The prior variance — represents the brain's initial uncertainty about
/// outcomes. A value of 4.0 means the brain initially expects outcomes to
/// vary by ±2 fans (std dev). This shrinks as the brain collects data.
pub const PRIOR_VARIANCE: f64 = 4.0;

/// The observation variance — the noise level the brain assumes for each
/// observation. Higher values make the brain more conservative (slower
/// to update). This can be made adaptive in the future.
const OBSERVATION_VARIANCE: f64 = 4.0;

/// Minimum number of paired treatment/control observations before the brain
/// trusts the treatment-effect posterior over the outcome model. Below this,
/// the brain falls back to the outcome model P(Y|action,context) because the
/// treatment-effect estimate is too noisy.
pub const MIN_TREATMENT_CONFIDENCE: u32 = 5;

/// The brain's causal model: P(incremental_fan | template, context).
///
/// Uses a `HierarchicalPosterior` for proper Bayesian learning with partial
/// pooling across templates and subreddit types. The hierarchical structure
/// means:
/// - Templates with many observations stand on their own.
/// - Templates with few observations shrink toward the global mean.
/// - Subreddit-type multipliers are learned and pooled.
///
/// Context adjustments (event proximity, growth trend) are applied
/// multiplicatively on top of the posterior mean, same as before.
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
    /// Hierarchical posterior for fan acquisition (outcome model).
    pub fans: HierarchicalPosterior,
    /// Treatment-effect posterior P(τ|context). Primary ranking signal when
    /// confidence is high; falls back to the outcome model when low.
    pub treatment_effects: TreatmentEffectPosterior,
    /// Per-template Signal install EMA. Learned independently from fan
    /// counts because Signal adoption has different drivers.
    pub template_expected_signal: HashMap<String, f64>,
}

/// Treatment-aware prediction statistics — the result of querying both the
/// outcome model and the treatment-effect model in a single call.
///
/// When `use_treatment_effect` is true, the EFE scorer should use
/// `treatment_effect` as the expected fans and `treatment_std` as the
/// uncertainty. When false, it should use `expected_fans` and `predict_std`
/// from the outcome model.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct TreatmentAwareStats {
    /// Expected fans from the outcome model (fallback).
    pub expected_fans: f64,
    /// Treatment effect τ(x) from the treatment-effect model (primary).
    pub treatment_effect: f64,
    /// Uncertainty in the treatment effect.
    pub treatment_std: f64,
    /// Uncertainty in the outcome model prediction.
    pub predict_std: f64,
    /// Outcome model confidence (observation count).
    pub confidence: u32,
    /// Treatment-effect model confidence (paired observation count).
    pub treatment_confidence: u32,
    /// Whether to use the treatment effect as the primary signal.
    pub use_treatment_effect: bool,
}

impl CausalModel {
    /// Creates a causal model with the default priors.
    #[must_use]
    pub fn new() -> Self {
        Self {
            fans: HierarchicalPosterior::new(NormalPosterior::prior(
                DEFAULT_EXPECTED_FANS,
                PRIOR_VARIANCE,
            )),
            treatment_effects: TreatmentEffectPosterior::new(),
            template_expected_signal: HashMap::new(),
        }
    }

    /// Predicts expected new fans for a dispatch given its context.
    ///
    /// Combines the hierarchical posterior mean with context adjustments:
    /// - Subreddit-type multiplier (learned from past outcomes via partial pooling)
    /// - Event proximity (≤7 days) boosts expected fans by 1.5x
    /// - Event proximity (≤30 days) boosts by 1.2x
    /// - Stagnant growth reduces expected fans by 0.8x
    /// - Accelerating growth boosts by 1.1x
    ///
    /// `post_format`, `time_of_day_bps`, and `community_novelty_bps` are
    /// carried by [`DispatchContext`] but not yet wired into the predictor —
    /// they are reserved for a future learned-context multiplier so the
    /// shape is stable before the coefficients exist.
    #[must_use]
    pub fn predict(&self, template_id: &str, context: &DispatchContext) -> f64 {
        let (mean, _var) = self
            .fans
            .predict(template_id, context.subreddit_type.as_deref());
        apply_context_adjustments(mean, context)
    }

    /// Predicts expected Signal installs for a dispatch. Uses the
    /// template-level Signal EMA if available, otherwise falls back to
    /// 10% of the fan prediction (a reasonable conversion prior).
    #[must_use]
    pub fn predict_signal(&self, template_id: &str, context: &DispatchContext) -> f64 {
        // Single HashMap lookup: if we have a learned Signal prior,
        // apply the same context adjustments as fans. Otherwise fall
        // back to 10% of the fan prediction.
        if let Some(&signal_prior) = self.template_expected_signal.get(template_id) {
            let mut prediction = signal_prior;
            if let Some(days) = context.days_to_event {
                if days <= 7 {
                    prediction *= 1.3;
                } else if days <= 30 {
                    prediction *= 1.1;
                }
            }
            // Growth trend modulates the Signal prediction, same as fans.
            match context.fan_growth_trend {
                GrowthTrend::Stagnant | GrowthTrend::Decelerating => prediction *= 0.8,
                GrowthTrend::Accelerating => prediction *= 1.1,
                GrowthTrend::Steady => {}
            }
            return prediction.max(0.0);
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
    /// Uses the conjugate Normal-Normal update via the hierarchical posterior.
    /// Both the fan count and Signal install models are updated independently.
    pub fn update(&mut self, outcome: &PredictionOutcome) {
        let template = &outcome.prediction.template_id;
        let subreddit_type = outcome.prediction.context.subreddit_type.as_deref();
        // Update the hierarchical fan posterior.
        self.fans.update(
            Some(template),
            subreddit_type,
            outcome.observed_new_fans,
            OBSERVATION_VARIANCE,
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
        let expected_fans = apply_context_adjustments(mean, context);
        let predict_std = var.sqrt().max(0.1);
        let confidence = self.fans.confidence(template_id);
        (expected_fans, predict_std, confidence)
    }

    /// Returns the expected fan count for a template, or the default prior.
    #[must_use]
    pub fn expected_fans(&self, template_id: &str) -> f64 {
        let post = self.fans.template_posterior(template_id);
        post.mean
    }

    /// Returns P(expected_fans > 0) for a template — the probability that
    /// this template produces any fans at all.
    #[must_use]
    pub fn p_positive(&self, template_id: &str) -> f64 {
        let post = self.fans.template_posterior(template_id);
        post.p_positive()
    }

    /// Predicts the heterogeneous treatment effect τ(x) for a template in
    /// the given context. Returns `(tau, std, confidence)`.
    ///
    /// Positive τ means the action increases fans; negative means it
    /// backfires. The confidence is the number of paired treatment/control
    /// observations used to estimate τ.
    #[must_use]
    pub fn predict_treatment_effect(
        &self,
        template_id: &str,
        context: &DispatchContext,
    ) -> (f64, f64, u32) {
        self.treatment_effects
            .predict_stats(template_id, context.subreddit_type.as_deref())
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

    /// Updates the treatment-effect posterior from credit-assigned episode
    /// outcomes.
    ///
    /// Each `CreditAssignment` represents the brain's estimate of how much
    /// of an episode's outcome was caused by one specific dispatch. The
    /// credit is used as the observed treatment effect τ for that template,
    /// with a variance that increases with the number of dispatches (more
    /// dispatches → more uncertainty in the attribution).
    ///
    /// This is the key link between the episode model (Phase 2) and the
    /// treatment-effect model (Phase 1): the episode records the full
    /// trajectory, credit assignment distributes the outcome across actions,
    /// and the treatment-effect posterior learns from the credited outcomes.
    pub fn update_with_credit(
        &mut self,
        credits: &[crate::opportunity::CreditAssignment],
        subreddit_type: Option<&str>,
    ) {
        for credit in credits {
            // Variance scales with the number of dispatches: more dispatches
            // means more uncertainty in the attribution. With 1 dispatch,
            // variance = OBSERVATION_VARIANCE. With N dispatches, variance =
            // OBSERVATION_VARIANCE * N.
            let n = credit.total_dispatches.max(1) as f64;
            let observation_variance = OBSERVATION_VARIANCE * n;
            self.treatment_effects.update(
                &credit.template_id,
                subreddit_type,
                credit.credit,
                observation_variance,
            );
        }
    }

    /// Returns treatment-aware prediction statistics — both the outcome model
    /// and the treatment-effect model in a single call.
    ///
    /// When `use_treatment_effect` is true, the EFE scorer should use
    /// `treatment_effect` as `expected_new_fans` and `treatment_std` as the
    /// uncertainty. When false, it falls back to the outcome model.
    ///
    /// The switch happens when `treatment_confidence >= MIN_TREATMENT_CONFIDENCE`.
    #[must_use]
    pub fn predict_stats_with_treatment(
        &self,
        template_id: &str,
        context: &DispatchContext,
    ) -> TreatmentAwareStats {
        let (expected_fans, predict_std, confidence) = self.predict_stats(template_id, context);
        let (treatment_effect, treatment_std, treatment_confidence) = self
            .treatment_effects
            .predict_stats(template_id, context.subreddit_type.as_deref());
        let use_treatment_effect = treatment_confidence >= MIN_TREATMENT_CONFIDENCE;
        TreatmentAwareStats {
            expected_fans,
            treatment_effect,
            treatment_std,
            predict_std,
            confidence,
            treatment_confidence,
            use_treatment_effect,
        }
    }
}

/// Applies the context adjustments (event proximity, growth trend) to a
/// raw posterior mean. This is the single source of truth for the
/// multiplicative context adjustments — both [`CausalModel::predict`] and
/// [`crate::simulation::WorldSimulation`] call this so the adjustments
/// never drift out of sync.
///
/// - Event proximity (≤7 days) boosts by 1.5x
/// - Event proximity (≤30 days) boosts by 1.2x
/// - Stagnant/Decelerating growth reduces by 0.8x
/// - Accelerating growth boosts by 1.1x
#[must_use]
pub(crate) fn apply_context_adjustments(mut prediction: f64, context: &DispatchContext) -> f64 {
    if let Some(days) = context.days_to_event {
        if days <= 7 {
            prediction *= 1.5;
        } else if days <= 30 {
            prediction *= 1.2;
        }
    }
    match context.fan_growth_trend {
        GrowthTrend::Stagnant | GrowthTrend::Decelerating => prediction *= 0.8,
        GrowthTrend::Accelerating => prediction *= 1.1,
        GrowthTrend::Steady => {}
    }
    prediction.max(0.0)
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
        // Unmeasured template: std = sqrt(PRIOR_VARIANCE) = 2.0.
        assert!((model.predict_std("t") - 2.0).abs() < 0.01);
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
        // Only 3 observations — below MIN_TREATMENT_CONFIDENCE (5).
        for _ in 0..3 {
            model.update_treatment_effect("t", None, 5.0, 1.0);
        }
        let ctx = DispatchContext::default();
        let stats = model.predict_stats_with_treatment("t", &ctx);
        assert!(!stats.use_treatment_effect);
        assert_eq!(stats.treatment_confidence, 3);
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
        for _ in 0..10 {
            model.update_treatment_effect("t", Some("metal"), 8.0, 1.0);
        }
        let ctx_metal = DispatchContext {
            subreddit_type: Some("metal".to_owned()),
            ..Default::default()
        };
        let ctx_none = DispatchContext::default();
        let stats_metal = model.predict_stats_with_treatment("t", &ctx_metal);
        let stats_none = model.predict_stats_with_treatment("t", &ctx_none);
        assert!(stats_metal.treatment_effect > stats_none.treatment_effect);
    }
}
