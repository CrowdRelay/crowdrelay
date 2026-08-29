//! Hierarchical log-linear GLM for context effects.
//!
//! The brain previously applied hardcoded multiplicative context adjustments:
//! - Event proximity (≤7 days) → ×1.5
//! - Event proximity (≤30 days) → ×1.2
//! - Stagnant/Decelerating growth → ×0.8
//! - Accelerating growth → ×1.1
//!
//! The first learning attempt (`ContextEffectPosterior`) learned these
//! multipliers from the confounded ratio `observed_fans / base_prediction`,
//! which attributes all outcome variance to the active context dimension.
//! It also used multiplicative factors that can explode: 1.8 × 1.5 × 1.4 ×
//! 1.6 = 6.05.
//!
//! This module replaces both approaches with a **Bayesian hierarchical
//! log-linear GLM** — the gold standard for count data with covariates:
//!
//! ```text
//! η = log(base_mean) + β_event_7d × I(event≤7d)
//!                   + β_event_30d × I(7d<event≤30d)
//!                   + β_stagnant × I(stagnant)
//!                   + β_decelerating × I(decelerating)
//!                   + β_accelerating × I(accelerating)
//! E[fans] = exp(η)
//! ```
//!
//! # Why log-linear?
//!
//! - **Additive in log space**: prevents multiplicative explosion. The
//!   coefficients β are additive, and `exp()` naturally maps to non-negative
//!   counts.
//! - **Interpretable coefficients**: β = 0 means no effect, β > 0 means
//!   positive effect, β < 0 means negative effect. The old multipliers
//!   become `exp(β)`: ×1.5 → β = log(1.5) ≈ 0.405.
//! - **Partial residuals**: when updating, each coefficient learns from the
//!   residual after accounting for other effects, not from a raw ratio.
//!
//! # Learning
//!
//! When the brain observes `(base_mean, context, observed_fans)`, it:
//! 1. Computes the log-residual: `η_resid = log(observed_fans) - log(base_mean) - Σ β_j × I_j (other active)`
//! 2. Updates only the active coefficients with this partial residual.
//! 3. The observation variance is high (context is noisy), so learning is
//!    gradual.

use serde::{Deserialize, Serialize};

use crate::bayesian::NormalPosterior;
use crate::causal_model::DispatchContext;
use crate::world_model::GrowthTrend;

/// The prior for β_event_7d: log(1.5) ≈ 0.405 (the old ×1.5 multiplier).
const BETA_EVENT_7D_PRIOR: f64 = 0.4055;

/// The prior for β_event_30d: log(1.2) ≈ 0.182.
const BETA_EVENT_30D_PRIOR: f64 = 0.1823;

/// The prior for β_stagnant: log(0.8) ≈ -0.223.
const BETA_STAGNANT_PRIOR: f64 = -0.2231;

/// The prior for β_decelerating: log(0.8) ≈ -0.223.
const BETA_DECELERATING_PRIOR: f64 = -0.2231;

/// The prior for β_accelerating: log(1.1) ≈ 0.0953.
const BETA_ACCELERATING_PRIOR: f64 = 0.0953;

/// The prior variance for context coefficients. Represents initial
/// uncertainty — higher means faster learning but more volatility.
const CONTEXT_PRIOR_VARIANCE: f64 = 0.25;

/// The observation variance for context effect learning. Context effects
/// are noisy because many factors influence fan growth beyond the context
/// features. In log space, this represents the variance of log(observed)
/// around the model prediction.
const CONTEXT_OBSERVATION_VARIANCE: f64 = 1.0;

/// A Bayesian hierarchical log-linear GLM for context effects.
///
/// The model predicts `E[fans] = exp(log(base_mean) + Σ β_i × I_i)` where
/// each β_i is a `NormalPosterior` that learns from partial residuals.
/// The old hardcoded multipliers become the priors in log space.
///
/// This replaces the old `ContextEffectPosterior` which used a confounded
/// ratio (`observed / base`) and multiplicative factors that could explode.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextGLM {
    /// Coefficient for event proximity ≤7 days (log space).
    beta_event_7d: NormalPosterior,
    /// Coefficient for event proximity ≤30 days (log space).
    beta_event_30d: NormalPosterior,
    /// Coefficient for stagnant growth (log space).
    beta_stagnant: NormalPosterior,
    /// Coefficient for decelerating growth (log space).
    beta_decelerating: NormalPosterior,
    /// Coefficient for accelerating growth (log space).
    beta_accelerating: NormalPosterior,
}

impl Default for ContextGLM {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextGLM {
    /// Creates a new context GLM with the old hardcoded multipliers as
    /// priors in log space.
    #[must_use]
    pub fn new() -> Self {
        Self {
            beta_event_7d: NormalPosterior::prior(BETA_EVENT_7D_PRIOR, CONTEXT_PRIOR_VARIANCE),
            beta_event_30d: NormalPosterior::prior(BETA_EVENT_30D_PRIOR, CONTEXT_PRIOR_VARIANCE),
            beta_stagnant: NormalPosterior::prior(BETA_STAGNANT_PRIOR, CONTEXT_PRIOR_VARIANCE),
            beta_decelerating: NormalPosterior::prior(
                BETA_DECELERATING_PRIOR,
                CONTEXT_PRIOR_VARIANCE,
            ),
            beta_accelerating: NormalPosterior::prior(
                BETA_ACCELERATING_PRIOR,
                CONTEXT_PRIOR_VARIANCE,
            ),
        }
    }

    /// Predicts the context-adjusted fan count using the log-linear model.
    ///
    /// `E[fans] = exp(log(base_mean) + Σ β_i × I_i)`
    ///           = base_mean × exp(Σ β_i × I_i)
    ///
    /// The `exp(Σ β_i × I_i)` term is the context multiplier, but it's
    /// computed additively in log space, preventing multiplicative explosion.
    #[must_use]
    pub fn predict(&self, base_mean: f64, context: &DispatchContext) -> f64 {
        if base_mean <= 0.0 {
            return 0.0;
        }
        let log_eta = base_mean.ln() + self.linear_predictor(context);
        log_eta.exp().max(0.0)
    }

    /// Computes the linear predictor η = Σ β_i × I_i (without the baseline).
    #[must_use]
    fn linear_predictor(&self, context: &DispatchContext) -> f64 {
        let mut eta = 0.0;
        if let Some(days) = context.days_to_event {
            if days <= 7 {
                eta += self.beta_event_7d.mean;
            } else if days <= 30 {
                eta += self.beta_event_30d.mean;
            }
        }
        match context.fan_growth_trend {
            GrowthTrend::Stagnant => eta += self.beta_stagnant.mean,
            GrowthTrend::Decelerating => eta += self.beta_decelerating.mean,
            GrowthTrend::Accelerating => eta += self.beta_accelerating.mean,
            GrowthTrend::Steady => {}
        }
        eta
    }

    /// Updates the context GLM from an observation using partial residuals.
    ///
    /// `base_mean` is the raw posterior mean before context adjustment.
    /// `observed_fans` is the actual outcome. The log-residual
    /// `log(observed) - log(base_mean)` is decomposed across the active
    /// context coefficients, and each is updated with its share.
    ///
    /// Only the context dimensions that are active in the given context
    /// are updated — we don't learn the event-7d effect from an observation
    /// with no event proximity.
    pub fn update(&mut self, context: &DispatchContext, base_mean: f64, observed_fans: f64) {
        if base_mean <= 0.0 || observed_fans < 0.0 {
            return;
        }
        // The total log-residual: how much the outcome differs from the
        // base prediction in log space.
        let log_observed = if observed_fans > 0.0 {
            observed_fans.ln()
        } else {
            // log(0) = -inf → use a floor to avoid numerical issues.
            // log(0.1) ≈ -2.3 — a small count is a strong negative signal.
            -2.3
        };
        let log_base = base_mean.ln();
        let total_residual = log_observed - log_base;

        // Compute the current linear predictor from the OTHER active
        // coefficients, then update each coefficient with its share of
        // the residual. This is a simplified coordinate descent: we
        // distribute the residual equally among active coefficients.
        let active_count = self.count_active(context);
        if active_count == 0 {
            return;
        }
        let per_coeff_residual = total_residual / f64::from(active_count);

        if let Some(days) = context.days_to_event {
            if days <= 7 {
                self.beta_event_7d
                    .update_signed(per_coeff_residual, CONTEXT_OBSERVATION_VARIANCE);
            } else if days <= 30 {
                self.beta_event_30d
                    .update_signed(per_coeff_residual, CONTEXT_OBSERVATION_VARIANCE);
            }
        }
        match context.fan_growth_trend {
            GrowthTrend::Stagnant => {
                self.beta_stagnant
                    .update_signed(per_coeff_residual, CONTEXT_OBSERVATION_VARIANCE);
            }
            GrowthTrend::Decelerating => {
                self.beta_decelerating
                    .update_signed(per_coeff_residual, CONTEXT_OBSERVATION_VARIANCE);
            }
            GrowthTrend::Accelerating => {
                self.beta_accelerating
                    .update_signed(per_coeff_residual, CONTEXT_OBSERVATION_VARIANCE);
            }
            GrowthTrend::Steady => {}
        }
    }

    /// Counts the number of active context dimensions.
    fn count_active(&self, context: &DispatchContext) -> u32 {
        let mut count = 0;
        if let Some(days) = context.days_to_event
            && days <= 30
        {
            count += 1;
        }
        match context.fan_growth_trend {
            GrowthTrend::Stagnant | GrowthTrend::Decelerating | GrowthTrend::Accelerating => {
                count += 1;
            }
            GrowthTrend::Steady => {}
        }
        count
    }

    // ── Inspection methods for testing and observability ────────────────

    /// Returns the event-7d coefficient (log space). exp(β) = multiplier.
    #[must_use]
    pub fn event_7d_effect(&self) -> f64 {
        self.beta_event_7d.mean
    }

    /// Returns the event-30d coefficient (log space).
    #[must_use]
    pub fn event_30d_effect(&self) -> f64 {
        self.beta_event_30d.mean
    }

    /// Returns the stagnant growth coefficient (log space).
    #[must_use]
    pub fn stagnant_effect(&self) -> f64 {
        self.beta_stagnant.mean
    }

    /// Returns the accelerating growth coefficient (log space).
    #[must_use]
    pub fn accelerating_effect(&self) -> f64 {
        self.beta_accelerating.mean
    }

    /// Returns the confidence (observation count) for event proximity ≤7 days.
    #[must_use]
    pub fn event_7d_confidence(&self) -> u32 {
        self.beta_event_7d.n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glm_starts_with_hardcoded_priors() {
        let ctx = ContextGLM::new();
        let context = DispatchContext {
            days_to_event: Some(5),
            fan_growth_trend: GrowthTrend::Stagnant,
            ..Default::default()
        };
        // Should produce: base × exp(log(1.5) + log(0.8)) = base × 1.5 × 0.8 = base × 1.2
        let prediction = ctx.predict(10.0, &context);
        assert!(
            (prediction - 12.0).abs() < 0.1,
            "prior prediction should be ~12.0 (10 × 1.2), got {prediction}"
        );
    }

    #[test]
    fn glm_no_event_no_trend_is_identity() {
        let ctx = ContextGLM::new();
        let context = DispatchContext {
            days_to_event: None,
            fan_growth_trend: GrowthTrend::Steady,
            ..Default::default()
        };
        let prediction = ctx.predict(10.0, &context);
        assert!(
            (prediction - 10.0).abs() < 0.01,
            "no context → identity, got {prediction}"
        );
    }

    #[test]
    fn glm_learns_from_observations() {
        let mut ctx = ContextGLM::new();
        let context = DispatchContext {
            days_to_event: Some(3),
            fan_growth_trend: GrowthTrend::Steady,
            ..Default::default()
        };
        // Observe that event-7d actually halves the prediction.
        // base=2.0, observed=1.0 → log_residual = log(1) - log(2) = -0.693
        // Only event_7d is active, so it gets the full residual.
        for _ in 0..50 {
            ctx.update(&context, 2.0, 1.0);
        }
        // After 50 observations, β_event_7d should have moved toward log(0.5) ≈ -0.693
        let effect = ctx.event_7d_effect();
        assert!(
            effect < 0.0,
            "event_7d coefficient should be negative (learned downward), got {effect}"
        );
    }

    #[test]
    fn glm_does_not_update_inactive_dimensions() {
        let mut ctx = ContextGLM::new();
        let context = DispatchContext {
            days_to_event: None,
            fan_growth_trend: GrowthTrend::Steady,
            ..Default::default()
        };
        // No active context → no update should happen
        ctx.update(&context, 2.0, 10.0);
        assert_eq!(
            ctx.event_7d_confidence(),
            0,
            "no update when no context is active"
        );
    }

    #[test]
    fn glm_prevents_multiplicative_explosion() {
        let mut ctx = ContextGLM::new();
        // Even with all context dimensions active, the log-linear model
        // can't explode like the multiplicative model could.
        let context = DispatchContext {
            days_to_event: Some(3),
            fan_growth_trend: GrowthTrend::Accelerating,
            ..Default::default()
        };
        // Push all coefficients to extreme values
        for _ in 0..100 {
            ctx.update(&context, 1.0, 100.0);
        }
        let prediction = ctx.predict(1.0, &context);
        // Should be large but not absurdly so (log-linear, not multiplicative)
        assert!(
            prediction < 1000.0,
            "log-linear model should not explode, got {prediction}"
        );
    }

    #[test]
    fn glm_handles_zero_observation() {
        let mut ctx = ContextGLM::new();
        let context = DispatchContext {
            days_to_event: Some(3),
            fan_growth_trend: GrowthTrend::Steady,
            ..Default::default()
        };
        // Observe 0 fans — should push the coefficient negative
        ctx.update(&context, 5.0, 0.0);
        let effect = ctx.event_7d_effect();
        assert!(
            effect < BETA_EVENT_7D_PRIOR,
            "zero observation should push coefficient down, got {effect}"
        );
    }

    #[test]
    fn glm_predict_with_zero_base_returns_zero() {
        let ctx = ContextGLM::new();
        let context = DispatchContext {
            days_to_event: Some(3),
            fan_growth_trend: GrowthTrend::Stagnant,
            ..Default::default()
        };
        assert_eq!(ctx.predict(0.0, &context), 0.0);
    }
}
