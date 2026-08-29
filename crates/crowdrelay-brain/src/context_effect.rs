//! Bayesian online log-linear GLM for context effects.
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
//! The second attempt (coordinate descent) distributed the log-residual
//! equally across active coefficients (`per_coeff = total_residual /
//! active_count`). This creates **attribution leakage** between correlated
//! covariates: when event proximity and trend are both active, both get the
//! same share of the surprise, regardless of which actually drove the
//! outcome.
//!
//! This module replaces both with a **Bayesian online log-linear GLM** using
//! a Laplace approximation with Fisher scoring (online Newton's method):
//!
//! ```text
//! η = log(base_mean) + X·β
//! y ~ Poisson(exp(η))
//! β ~ Normal(μ₀, Σ₀)   (prior)
//! ```
//!
//! The posterior over β is approximated as a multivariate Normal with mean
//! vector `μ` (5-dim) and covariance matrix `Σ` (5×5). Each observation
//! updates the posterior via one Newton step:
//!
//! ```text
//! g = x·(y - exp(η))           (score / gradient)
//! H = Σ⁻¹ + exp(η)·(x⊗x)      (Fisher info + prior precision)
//! μ_new = μ + H⁻¹·g            (Newton step)
//! Σ_new = H⁻¹                  (posterior covariance)
//! ```
//!
//! This correctly attributes the outcome to the covariates that actually
//! drove it, because the gradient `g` is proportional to `x` (the design
//! vector), and the Hessian `H` accounts for the correlation between
//! covariates through the outer product `x⊗x`.
//!
//! # Why log-linear?
//!
//! - **Additive in log space**: prevents multiplicative explosion.
//! - **Interpretable coefficients**: β = 0 means no effect. The old
//!   multipliers become `exp(β)`: ×1.5 → β = log(1.5) ≈ 0.405.
//! - **Joint uncertainty**: the 5×5 covariance matrix captures correlations
//!   between context effects, giving honest predictive uncertainty.

use serde::{Deserialize, Serialize};

use crate::causal_model::DispatchContext;
use crate::world_model::GrowthTrend;

/// Number of context coefficients in the GLM.
const N_COEFFS: usize = 5;

// Coefficient indices:
const IDX_EVENT_7D: usize = 0;
const IDX_EVENT_30D: usize = 1;
const IDX_STAGNANT: usize = 2;
const IDX_DECELERATING: usize = 3;
const IDX_ACCELERATING: usize = 4;

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

/// Small ridge added to the Hessian for numerical stability. Ensures the
/// matrix is positive definite even with extreme observations.
const RIDGE_EPS: f64 = 1e-6;

/// Maximum Newton step size per coefficient per observation. Prevents the
/// Newton step from overshooting when the residual is large (e.g. observing
/// 100 fans when the prediction was 2). The step is scaled down if any
/// coefficient would move more than this.
const MAX_STEP: f64 = 0.5;

/// A Bayesian online log-linear GLM for context effects.
///
/// The model predicts `E[fans] = exp(log(base_mean) + xᵀβ)` where `β` is a
/// 5-dimensional coefficient vector with a multivariate Normal posterior.
/// The posterior is updated via online Laplace approximation (Fisher
/// scoring / Newton's method), which correctly attributes outcomes to the
/// covariates that drove them — unlike the old equal-residual-splitting
/// coordinate descent.
///
/// The 5×5 covariance matrix captures correlations between context effects,
/// giving honest predictive uncertainty via `predict_std`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextGLM {
    /// Posterior mean vector (5-dim).
    mu: [f64; N_COEFFS],
    /// Posterior covariance matrix (5×5).
    sigma: [[f64; N_COEFFS]; N_COEFFS],
    /// Number of observations used to form this posterior.
    n: u32,
}

impl Default for ContextGLM {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextGLM {
    /// Creates a new context GLM with the old hardcoded multipliers as
    /// priors in log space, and a diagonal prior covariance.
    #[must_use]
    pub fn new() -> Self {
        let mu = [
            BETA_EVENT_7D_PRIOR,
            BETA_EVENT_30D_PRIOR,
            BETA_STAGNANT_PRIOR,
            BETA_DECELERATING_PRIOR,
            BETA_ACCELERATING_PRIOR,
        ];
        let sigma = [
            [CONTEXT_PRIOR_VARIANCE, 0.0, 0.0, 0.0, 0.0],
            [0.0, CONTEXT_PRIOR_VARIANCE, 0.0, 0.0, 0.0],
            [0.0, 0.0, CONTEXT_PRIOR_VARIANCE, 0.0, 0.0],
            [0.0, 0.0, 0.0, CONTEXT_PRIOR_VARIANCE, 0.0],
            [0.0, 0.0, 0.0, 0.0, CONTEXT_PRIOR_VARIANCE],
        ];
        Self { mu, sigma, n: 0 }
    }

    /// Builds the design vector `x` (binary indicators) from a dispatch context.
    #[must_use]
    fn design_vector(context: &DispatchContext) -> [f64; N_COEFFS] {
        let mut x = [0.0; N_COEFFS];
        if let Some(days) = context.days_to_event {
            if days <= 7 {
                x[IDX_EVENT_7D] = 1.0;
            } else if days <= 30 {
                x[IDX_EVENT_30D] = 1.0;
            }
        }
        match context.fan_growth_trend {
            GrowthTrend::Stagnant => x[IDX_STAGNANT] = 1.0,
            GrowthTrend::Decelerating => x[IDX_DECELERATING] = 1.0,
            GrowthTrend::Accelerating => x[IDX_ACCELERATING] = 1.0,
            GrowthTrend::Steady => {}
        }
        x
    }

    /// Returns true if any context dimension is active.
    fn has_active_context(x: &[f64; N_COEFFS]) -> bool {
        x.iter().any(|&v| v != 0.0)
    }

    /// Predicts the context-adjusted fan count using the log-linear model.
    ///
    /// `E[fans] = exp(log(base_mean) + xᵀμ)`
    ///           = base_mean × exp(xᵀμ)
    #[must_use]
    pub fn predict(&self, base_mean: f64, context: &DispatchContext) -> f64 {
        if base_mean <= 0.0 {
            return 0.0;
        }
        let x = Self::design_vector(context);
        let log_eta = base_mean.ln() + dot(&x, &self.mu);
        log_eta.exp().max(0.0)
    }

    /// Returns the context-aware predictive uncertainty: `sqrt(xᵀΣx)`.
    /// This is the standard deviation of the linear predictor due to
    /// uncertainty in β. It varies by context — a context with more active
    /// dimensions has more uncertainty.
    #[must_use]
    pub fn predict_std(&self, context: &DispatchContext) -> f64 {
        let x = Self::design_vector(context);
        let var = quadratic_form(&x, &self.sigma);
        var.sqrt().max(0.01)
    }

    /// Updates the GLM posterior from one observation using online Laplace
    /// approximation (Fisher scoring / Newton's method).
    ///
    /// `base_mean` is the raw posterior mean before context adjustment.
    /// `observed_fans` is the actual count outcome. The update computes the
    /// Poisson log-likelihood gradient and Fisher information, then performs
    /// one Newton step on the posterior over β.
    ///
    /// Only observations with at least one active context dimension update
    /// the posterior — we don't learn context effects from observations
    /// with no context.
    pub fn update(&mut self, context: &DispatchContext, base_mean: f64, observed_fans: f64) {
        if base_mean <= 0.0 || observed_fans < 0.0 {
            return;
        }
        let x = Self::design_vector(context);
        if !Self::has_active_context(&x) {
            return;
        }

        // Predicted count: η = log(base_mean) + xᵀμ, μ_pred = exp(η)
        let eta = base_mean.ln() + dot(&x, &self.mu);
        let mu_pred = eta.exp();

        // Score (gradient of log-likelihood): g = x · (y - μ_pred)
        let residual = observed_fans - mu_pred;
        let mut g = [0.0; N_COEFFS];
        for i in 0..N_COEFFS {
            g[i] = x[i] * residual;
        }

        // Prior precision: Λ = Σ⁻¹
        let lambda = mat_inv_5x5(&self.sigma);

        // Fisher information (Hessian): H = Λ + μ_pred · (x ⊗ x)
        // Add a small ridge for numerical stability.
        let mut h = lambda;
        for i in 0..N_COEFFS {
            for j in 0..N_COEFFS {
                h[i][j] += mu_pred * x[i] * x[j];
                if i == j {
                    h[i][j] += RIDGE_EPS;
                }
            }
        }

        // Solve H·δ = g → δ = H⁻¹·g
        let h_inv = mat_inv_5x5(&h);
        let mut delta = [0.0; N_COEFFS];
        for i in 0..N_COEFFS {
            for j in 0..N_COEFFS {
                delta[i] += h_inv[i][j] * g[j];
            }
        }

        // Clamp the step size to prevent overshooting. If any coefficient
        // would move more than MAX_STEP, scale the entire step down. This
        // is a simple line-search substitute that keeps the Newton step in
        // the region where the quadratic approximation is valid.
        let max_abs_delta = delta.iter().fold(0.0f64, |a, &b| a.max(b.abs()));
        if max_abs_delta > MAX_STEP {
            let scale = MAX_STEP / max_abs_delta;
            for d in &mut delta {
                *d *= scale;
            }
        }

        // Newton update: μ_new = μ + δ, Σ_new = H⁻¹
        for (i, d) in delta.iter().enumerate() {
            self.mu[i] += d;
        }
        self.sigma = h_inv;
        self.n += 1;
    }

    // ── Inspection methods for testing and observability ────────────────

    /// Returns the event-7d coefficient (log space). exp(β) = multiplier.
    #[must_use]
    pub fn event_7d_effect(&self) -> f64 {
        self.mu[IDX_EVENT_7D]
    }
}

// ── 5×5 matrix operations ────────────────────────────────────────────────

/// Dot product of two 5-dim vectors.
fn dot(a: &[f64; N_COEFFS], b: &[f64; N_COEFFS]) -> f64 {
    let mut s = 0.0;
    for i in 0..N_COEFFS {
        s += a[i] * b[i];
    }
    s
}

/// Quadratic form: xᵀ·M·x for a 5-dim vector and 5×5 matrix.
fn quadratic_form(x: &[f64; N_COEFFS], m: &[[f64; N_COEFFS]; N_COEFFS]) -> f64 {
    let mut s = 0.0;
    for i in 0..N_COEFFS {
        for j in 0..N_COEFFS {
            s += x[i] * m[i][j] * x[j];
        }
    }
    s
}

/// Inverts a 5×5 matrix using Gauss-Jordan elimination with partial pivoting.
/// Returns the identity if the matrix is singular (should not happen with the
/// ridge regularization).
#[must_use]
fn mat_inv_5x5(m: &[[f64; N_COEFFS]; N_COEFFS]) -> [[f64; N_COEFFS]; N_COEFFS] {
    // Augmented matrix [m | I]
    let mut aug = [[0.0f64; 2 * N_COEFFS]; N_COEFFS];
    for (i, row) in aug.iter_mut().enumerate() {
        row[..N_COEFFS].copy_from_slice(&m[i]);
        row[N_COEFFS + i] = 1.0;
    }

    for col in 0..N_COEFFS {
        // Partial pivoting: find the largest element in this column.
        let mut pivot = col;
        let mut max_val = aug[col][col].abs();
        for (row_idx, row) in aug.iter().enumerate().take(N_COEFFS).skip(col + 1) {
            if row[col].abs() > max_val {
                max_val = row[col].abs();
                pivot = row_idx;
            }
        }
        // Swap rows.
        if pivot != col {
            aug.swap(col, pivot);
        }

        // Check for singularity.
        if max_val < 1e-12 {
            // Return identity — shouldn't happen with ridge.
            let mut id = [[0.0; N_COEFFS]; N_COEFFS];
            for (i, row) in id.iter_mut().enumerate() {
                row[i] = 1.0;
            }
            return id;
        }

        // Scale pivot row.
        let pivot_val = aug[col][col];
        for val in aug[col].iter_mut() {
            *val /= pivot_val;
        }

        // Eliminate all other rows.
        for row in 0..N_COEFFS {
            if row == col {
                continue;
            }
            let factor = aug[row][col];
            if factor == 0.0 {
                continue;
            }
            let pivot_row = aug[col];
            for (val, &pv) in aug[row].iter_mut().zip(pivot_row.iter()) {
                *val -= factor * pv;
            }
        }
    }

    // Extract the inverse from the right half.
    let mut inv = [[0.0f64; N_COEFFS]; N_COEFFS];
    for (i, row) in aug.iter().enumerate() {
        inv[i].copy_from_slice(&row[N_COEFFS..(N_COEFFS + N_COEFFS)]);
    }
    inv
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
        // base=2.0, observed=1.0 → the model should learn β_event_7d < 0.
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
        assert_eq!(ctx.n, 0, "no update when no context is active");
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

    // ── Newton/Laplace attribution tests (P0.2) ──────────────────────────

    #[test]
    fn glm_newton_attributes_correctly() {
        // When two covariates are sometimes active together and sometimes
        // not, the Newton update should attribute the effect to the correct
        // one. With non-collinear design vectors, the Hessian's off-diagonal
        // terms allow the model to distinguish the effects.
        //
        // True model: β_event_7d = log(3) ≈ 1.099 (strong positive),
        // β_stagnant = 0 (no effect beyond the prior).
        let mut ctx = ContextGLM::new();

        // Phase 1: event_7d only (no stagnant) → teaches β_event_7d.
        let ctx_event_only = DispatchContext {
            days_to_event: Some(3),
            fan_growth_trend: GrowthTrend::Steady,
            ..Default::default()
        };
        // Phase 2: stagnant only (no event) → teaches β_stagnant = 0.
        let ctx_stagnant_only = DispatchContext {
            days_to_event: None,
            fan_growth_trend: GrowthTrend::Stagnant,
            ..Default::default()
        };
        // Phase 3: both active → should be consistent with what was learned.
        let ctx_both = DispatchContext {
            days_to_event: Some(3),
            fan_growth_trend: GrowthTrend::Stagnant,
            ..Default::default()
        };

        let _true_beta_event = 3.0f64.ln(); // ≈ 1.099
        for _ in 0..100 {
            let base = 2.0;
            // Event only: observed = base × 3.0
            ctx.update(&ctx_event_only, base, base * 3.0);
            // Stagnant only: observed = base × 1.0 (stagnant has no effect)
            ctx.update(&ctx_stagnant_only, base, base * 1.0);
            // Both: observed = base × 3.0 (event drives, stagnant doesn't)
            ctx.update(&ctx_both, base, base * 3.0);
        }

        let event_effect = ctx.event_7d_effect();
        let stagnant_effect = ctx.mu[IDX_STAGNANT];

        // β_event_7d should have moved significantly toward log(3).
        assert!(
            event_effect > BETA_EVENT_7D_PRIOR + 0.3,
            "event_7d should have learned a strong positive effect, got {event_effect}"
        );
        // β_stagnant should be close to 0 (no effect), not its prior of -0.223.
        // It should have moved UP from the prior toward 0.
        assert!(
            stagnant_effect > BETA_STAGNANT_PRIOR,
            "stagnant should have moved toward 0 (no effect), got {stagnant_effect}"
        );
        // The key attribution test: event_7d should have moved MUCH more
        // than stagnant, because event_7d is the true driver.
        let event_movement = (event_effect - BETA_EVENT_7D_PRIOR).abs();
        let stagnant_movement = (stagnant_effect - BETA_STAGNANT_PRIOR).abs();
        assert!(
            event_movement > stagnant_movement * 2.0,
            "event_7d should move more than stagnant (true driver), got event_movement={event_movement:.3} stagnant_movement={stagnant_movement:.3}"
        );
    }

    #[test]
    fn glm_converges_to_true_coefficients() {
        // Generate synthetic data from known β and verify convergence.
        let mut ctx = ContextGLM::new();
        let context = DispatchContext {
            days_to_event: Some(5),
            fan_growth_trend: GrowthTrend::Steady,
            ..Default::default()
        };
        // True β_event_7d = log(2) ≈ 0.693
        // Observed = base × 2.0
        let true_multiplier: f64 = 2.0;
        let true_beta = true_multiplier.ln();
        for _ in 0..500 {
            let base = 3.0;
            let observed = base * true_multiplier;
            ctx.update(&context, base, observed);
        }
        let learned = ctx.event_7d_effect();
        assert!(
            (learned - true_beta).abs() < 0.15,
            "should converge to true β={true_beta:.3}, got {learned:.3}"
        );
    }

    #[test]
    fn glm_predict_std_is_context_aware() {
        let ctx = ContextGLM::new();
        // No context → only prior variance of the intercept (which is 0
        // since no coefficients are active). The std should be ~0.
        let ctx_none = DispatchContext {
            days_to_event: None,
            fan_growth_trend: GrowthTrend::Steady,
            ..Default::default()
        };
        let std_none = ctx.predict_std(&ctx_none);

        // Two active dimensions → more uncertainty.
        let ctx_both = DispatchContext {
            days_to_event: Some(3),
            fan_growth_trend: GrowthTrend::Stagnant,
            ..Default::default()
        };
        let std_both = ctx.predict_std(&ctx_both);

        assert!(
            std_both > std_none,
            "context with more active dimensions should have higher uncertainty, got none={std_none:.4} both={std_both:.4}"
        );
    }

    #[test]
    fn glm_predict_std_decreases_with_observations() {
        let mut ctx = ContextGLM::new();
        let context = DispatchContext {
            days_to_event: Some(3),
            fan_growth_trend: GrowthTrend::Steady,
            ..Default::default()
        };
        let std_before = ctx.predict_std(&context);
        for _ in 0..100 {
            ctx.update(&context, 2.0, 3.0);
        }
        let std_after = ctx.predict_std(&context);
        assert!(
            std_after < std_before,
            "uncertainty should decrease with observations, got before={std_before:.4} after={std_after:.4}"
        );
    }

    #[test]
    fn mat_inv_5x5_identity() {
        let id = [
            [1.0, 0.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 0.0, 1.0],
        ];
        let inv = mat_inv_5x5(&id);
        for (i, row) in inv.iter().enumerate() {
            for (j, &val) in row.iter().enumerate() {
                let expected = if i == j { 1.0 } else { 0.0 };
                assert!((val - expected).abs() < 1e-10, "identity inverse");
            }
        }
    }

    #[test]
    fn mat_inv_5x5_diagonal() {
        let d = [
            [4.0, 0.0, 0.0, 0.0, 0.0],
            [0.0, 2.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 0.0, 0.5, 0.0],
            [0.0, 0.0, 0.0, 0.0, 0.25],
        ];
        let inv = mat_inv_5x5(&d);
        assert!((inv[0][0] - 0.25).abs() < 1e-10);
        assert!((inv[1][1] - 0.5).abs() < 1e-10);
        assert!((inv[2][2] - 1.0).abs() < 1e-10);
        assert!((inv[3][3] - 2.0).abs() < 1e-10);
        assert!((inv[4][4] - 4.0).abs() < 1e-10);
    }
}
