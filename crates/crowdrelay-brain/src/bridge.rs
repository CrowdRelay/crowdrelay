//! The Y14 → Y30 bridge — what a 14-day signal says about a 30-day one.
//!
//! Lives apart from the causal model because it answers a different
//! question. The causal model asks what an action is worth; the bridge asks
//! how much of a fortnight's growth is still there a month later, and how
//! sure we are about the answer.

use serde::{Deserialize, Serialize};

use crate::causal_model::MIN_BRIDGE_CONFIDENCE;

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
    ///
    /// When the bridge has few paired observations (`confidence < 10`), the
    /// variance is inflated to reflect that the bridge is still a guess.
    /// At confidence=0: 3× variance. At confidence≥10: 1× variance.
    /// This prevents the brain from treating Y14-bridged Y30 estimates as
    /// reliable when the bridge hasn't been calibrated yet.
    #[must_use]
    pub fn predict(&self, y14: f64) -> (f64, f64) {
        let x = [1.0, y14];
        let mean = x[0] * self.mu[0] + x[1] * self.mu[1];
        // Predictive variance: xᵀΣx + σ²_residual
        let var = x[0] * x[0] * self.sigma[0][0]
            + 2.0 * x[0] * x[1] * self.sigma[0][1]
            + x[1] * x[1] * self.sigma[1][1]
            + self.residual_variance;
        // Confidence-based variance inflation: 3× at n=0, 1× at n≥10.
        let confidence_factor = (self.n as f64 / MIN_BRIDGE_CONFIDENCE as f64).min(1.0);
        let inflation = 1.0 + (1.0 - confidence_factor) * 2.0;
        (mean, (var * inflation).max(0.01))
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
