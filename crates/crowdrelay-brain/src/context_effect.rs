//! Learned context effects — replace hardcoded multipliers with Bayesian
//! posteriors.
//!
//! The brain previously applied hardcoded context adjustments:
//! - Event proximity (≤7 days) → ×1.5
//! - Event proximity (≤30 days) → ×1.2
//! - Stagnant/Decelerating growth → ×0.8
//! - Accelerating growth → ×1.1
//!
//! These hardcoded beliefs can fight the Bayesian learner: if data says
//! event proximity is bad but the code says ×1.5, the posterior and the
//! engineering assumption pull in opposite directions.
//!
//! This module replaces the hardcoded multipliers with learned
//! `NormalPosterior` parameters. Each context dimension gets its own
//! posterior that learns the multiplicative effect from data. When
//! confidence is low (few observations), the posterior falls back to the
//! current hardcoded values as priors — so the brain starts with the same
//! behavior and gradually learns.

use serde::{Deserialize, Serialize};

use crate::bayesian::NormalPosterior;
use crate::causal_model::DispatchContext;
use crate::world_model::GrowthTrend;

/// The prior multiplier for event proximity ≤7 days. Used as the prior mean
/// for the learned posterior.
const EVENT_PROXIMITY_7D_PRIOR: f64 = 1.5;

/// The prior multiplier for event proximity ≤30 days.
const EVENT_PROXIMITY_30D_PRIOR: f64 = 1.2;

/// The prior multiplier for stagnant/decelerating growth.
const STAGNANT_GROWTH_PRIOR: f64 = 0.8;

/// The prior multiplier for accelerating growth.
const ACCELERATING_GROWTH_PRIOR: f64 = 1.1;

/// The prior variance — represents initial uncertainty about context effects.
/// Higher variance means the brain learns faster from data but is more
/// volatile with few observations.
const CONTEXT_PRIOR_VARIANCE: f64 = 0.25;

/// The observation variance for context effect learning. Context effects
/// are noisy because many factors influence fan growth beyond the context
/// features. Higher variance = more conservative learning.
const CONTEXT_OBSERVATION_VARIANCE: f64 = 4.0;

/// The learned multiplicative effect of context features on fan acquisition.
///
/// Each context dimension (event proximity, growth trend) has its own
/// `NormalPosterior` that learns the multiplier from data. The posteriors
/// are initialized with the current hardcoded values as priors, so the
/// brain starts with the same behavior and gradually learns the true
/// effects.
///
/// # Learning
///
/// When the brain observes an outcome, it computes the implied context
/// multiplier: `implied_multiplier = observed_fans / base_prediction`.
/// This is noisy (many factors affect fan growth), so the observation
/// variance is high. Over many observations, the posterior converges
/// toward the true context effect.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextEffectPosterior {
    /// Multiplier for event proximity ≤7 days.
    event_7d: NormalPosterior,
    /// Multiplier for event proximity ≤30 days (but >7).
    event_30d: NormalPosterior,
    /// Multiplier for stagnant growth.
    stagnant: NormalPosterior,
    /// Multiplier for decelerating growth.
    decelerating: NormalPosterior,
    /// Multiplier for accelerating growth.
    accelerating: NormalPosterior,
}

impl Default for ContextEffectPosterior {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextEffectPosterior {
    /// Creates a new context effect posterior with the hardcoded values
    /// as priors.
    #[must_use]
    pub fn new() -> Self {
        Self {
            event_7d: NormalPosterior::prior(EVENT_PROXIMITY_7D_PRIOR, CONTEXT_PRIOR_VARIANCE),
            event_30d: NormalPosterior::prior(EVENT_PROXIMITY_30D_PRIOR, CONTEXT_PRIOR_VARIANCE),
            stagnant: NormalPosterior::prior(STAGNANT_GROWTH_PRIOR, CONTEXT_PRIOR_VARIANCE),
            decelerating: NormalPosterior::prior(STAGNANT_GROWTH_PRIOR, CONTEXT_PRIOR_VARIANCE),
            accelerating: NormalPosterior::prior(ACCELERATING_GROWTH_PRIOR, CONTEXT_PRIOR_VARIANCE),
        }
    }

    /// Predicts the multiplicative context adjustment for a given context.
    ///
    /// Returns the product of all applicable context multipliers. Each
    /// multiplier is the posterior mean of the corresponding context
    /// dimension. When the posterior has little data, it falls back to
    /// the prior (the old hardcoded value).
    #[must_use]
    pub fn predict(&self, context: &DispatchContext) -> f64 {
        let mut multiplier = 1.0;
        if let Some(days) = context.days_to_event {
            if days <= 7 {
                multiplier *= self.event_7d.mean;
            } else if days <= 30 {
                multiplier *= self.event_30d.mean;
            }
        }
        match context.fan_growth_trend {
            GrowthTrend::Stagnant => multiplier *= self.stagnant.mean,
            GrowthTrend::Decelerating => multiplier *= self.decelerating.mean,
            GrowthTrend::Accelerating => multiplier *= self.accelerating.mean,
            GrowthTrend::Steady => {}
        }
        multiplier.max(0.0)
    }

    /// Updates the context effect posteriors from an observation.
    ///
    /// `base_prediction` is the raw posterior mean before context
    /// adjustment. `observed_fans` is the actual outcome. The implied
    /// multiplier is `observed_fans / base_prediction`, which is noisy
    /// but informative over many observations.
    ///
    /// Only the context dimensions that are active in the given context
    /// are updated — we don't learn the event-7d effect from an
    /// observation with no event proximity.
    pub fn update(&mut self, context: &DispatchContext, base_prediction: f64, observed_fans: f64) {
        if base_prediction <= 0.0 {
            return;
        }
        let implied_multiplier = observed_fans / base_prediction;
        // Clamp to a reasonable range to avoid outlier corruption.
        let clamped = implied_multiplier.clamp(0.0, 10.0);
        if let Some(days) = context.days_to_event {
            if days <= 7 {
                self.event_7d.update(clamped, CONTEXT_OBSERVATION_VARIANCE);
            } else if days <= 30 {
                self.event_30d.update(clamped, CONTEXT_OBSERVATION_VARIANCE);
            }
        }
        match context.fan_growth_trend {
            GrowthTrend::Stagnant => {
                self.stagnant.update(clamped, CONTEXT_OBSERVATION_VARIANCE);
            }
            GrowthTrend::Decelerating => {
                self.decelerating
                    .update(clamped, CONTEXT_OBSERVATION_VARIANCE);
            }
            GrowthTrend::Accelerating => {
                self.accelerating
                    .update(clamped, CONTEXT_OBSERVATION_VARIANCE);
            }
            GrowthTrend::Steady => {}
        }
    }

    /// Returns the posterior mean for event proximity ≤7 days.
    #[must_use]
    pub fn event_7d_effect(&self) -> f64 {
        self.event_7d.mean
    }

    /// Returns the posterior mean for event proximity ≤30 days.
    #[must_use]
    pub fn event_30d_effect(&self) -> f64 {
        self.event_30d.mean
    }

    /// Returns the posterior mean for stagnant growth.
    #[must_use]
    pub fn stagnant_effect(&self) -> f64 {
        self.stagnant.mean
    }

    /// Returns the posterior mean for accelerating growth.
    #[must_use]
    pub fn accelerating_effect(&self) -> f64 {
        self.accelerating.mean
    }

    /// Returns the confidence (observation count) for event proximity ≤7 days.
    #[must_use]
    pub fn event_7d_confidence(&self) -> u32 {
        self.event_7d.n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_effect_starts_with_hardcoded_priors() {
        let ctx = ContextEffectPosterior::new();
        let context = DispatchContext {
            days_to_event: Some(5),
            fan_growth_trend: GrowthTrend::Stagnant,
            ..Default::default()
        };
        // Should multiply 1.5 (event 7d) × 0.8 (stagnant) = 1.2
        let adjustment = ctx.predict(&context);
        assert!((adjustment - 1.2).abs() < 0.01);
    }

    #[test]
    fn context_effect_no_event_no_trend_is_identity() {
        let ctx = ContextEffectPosterior::new();
        let context = DispatchContext {
            days_to_event: None,
            fan_growth_trend: GrowthTrend::Steady,
            ..Default::default()
        };
        assert!((ctx.predict(&context) - 1.0).abs() < 0.001);
    }

    #[test]
    fn context_effect_learns_from_observations() {
        let mut ctx = ContextEffectPosterior::new();
        let context = DispatchContext {
            days_to_event: Some(3),
            fan_growth_trend: GrowthTrend::Steady,
            ..Default::default()
        };
        // Observe that event-7d actually halves the prediction.
        // base_prediction=2.0, observed=1.0 → implied_multiplier=0.5
        for _ in 0..50 {
            ctx.update(&context, 2.0, 1.0);
        }
        // After 50 observations, the posterior should have moved
        // significantly toward 0.5 from the prior of 1.5.
        let effect = ctx.event_7d_effect();
        assert!(
            effect < 1.0,
            "event_7d effect should have learned downward: got {effect}"
        );
    }

    #[test]
    fn context_effect_does_not_update_inactive_dimensions() {
        let mut ctx = ContextEffectPosterior::new();
        let context = DispatchContext {
            days_to_event: None,
            fan_growth_trend: GrowthTrend::Steady,
            ..Default::default()
        };
        ctx.update(&context, 2.0, 10.0);
        // No context dimensions active → no updates.
        assert_eq!(ctx.event_7d_confidence(), 0);
    }

    #[test]
    fn context_effect_clamps_outliers() {
        let mut ctx = ContextEffectPosterior::new();
        let context = DispatchContext {
            days_to_event: Some(3),
            fan_growth_trend: GrowthTrend::Steady,
            ..Default::default()
        };
        // base_prediction=0.01, observed=100 → implied=10000, clamped to 10.
        ctx.update(&context, 0.01, 100.0);
        // Should not corrupt the posterior.
        let effect = ctx.event_7d_effect();
        assert!(effect < 5.0, "outlier should be clamped: got {effect}");
    }

    #[test]
    fn context_effect_steady_trend_no_adjustment() {
        let ctx = ContextEffectPosterior::new();
        let context = DispatchContext {
            days_to_event: None,
            fan_growth_trend: GrowthTrend::Steady,
            ..Default::default()
        };
        assert!((ctx.predict(&context) - 1.0).abs() < 0.001);
    }

    #[test]
    fn context_effect_decelerating_uses_own_posterior() {
        let ctx = ContextEffectPosterior::new();
        let context = DispatchContext {
            days_to_event: None,
            fan_growth_trend: GrowthTrend::Decelerating,
            ..Default::default()
        };
        // Decelerating prior is 0.8 (same as stagnant).
        assert!((ctx.predict(&context) - 0.8).abs() < 0.01);
    }
}
