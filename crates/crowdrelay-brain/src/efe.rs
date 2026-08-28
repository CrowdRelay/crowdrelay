//! Opportunity Queue + Expected Free Energy (EFE) scoring.
//!
//! The brain doesn't just dispatch on a timer — it evaluates opportunities
//! and prioritizes them by Expected Free Energy (EFE), an Active Inference
//! metric that balances expected fan growth (pragmatic value) against
//! information gain (epistemic value).
//!
//! # EFE formula
//!
//! ```text
//! EFE = -(w_pragmatic * expected_fans
//!       + w_epistemic * information_gain * predict_std
//!       + w_exploration * novelty)
//!       + w_risk * predict_std
//! ```
//!
//! The brain minimizes EFE. Lower EFE = better opportunity.

use serde::Serialize;

use crate::causal_model::DispatchContext;

/// Configurable weights for EFE scoring.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct EfeWeights {
    /// Weight for expected fan growth (pragmatic value).
    pub pragmatic: f64,
    /// Weight for information gain × prediction uncertainty (epistemic value).
    pub epistemic: f64,
    /// Weight for exploration novelty (Go-Explore bonus).
    pub exploration: f64,
    /// Weight for risk penalty (variance aversion).
    pub risk: f64,
}

impl Default for EfeWeights {
    fn default() -> Self {
        Self {
            pragmatic: 1.0,
            epistemic: 0.5,
            exploration: 0.3,
            risk: 0.1,
        }
    }
}

/// A growth opportunity the brain has identified.
#[derive(Clone, Debug, Serialize)]
pub struct GrowthOpportunity {
    /// The worker template that would address this opportunity.
    pub template_id: String,
    /// Human-readable description of the opportunity.
    pub description: String,
    /// The expected fan growth if this opportunity is pursued.
    pub expected_fans: f64,
    /// The information gain — how much the brain would learn.
    pub information_gain: f64,
    /// The Expected Free Energy score: lower EFE = better opportunity.
    pub efe_score: f64,
    /// Context that informed this opportunity's scoring.
    pub context: DispatchContext,
    /// Why the brain identified this opportunity.
    pub reason: String,
}

impl GrowthOpportunity {
    /// Computes the EFE score using the full formula with uncertainty-weighted
    /// epistemic value and risk sensitivity.
    ///
    /// EFE = -(w_prag * expected_fans
    ///       + w_epist * information_gain * predict_std
    ///       + w_explore * novelty)
    ///       + w_risk * predict_std
    #[must_use]
    pub fn compute_efe(
        expected_fans: f64,
        information_gain: f64,
        predict_std: f64,
        novelty: f64,
        weights: EfeWeights,
    ) -> f64 {
        let pragmatic = weights.pragmatic * expected_fans;
        let epistemic = weights.epistemic * information_gain * predict_std;
        let exploration = weights.exploration * novelty;
        let risk = weights.risk * predict_std;
        -(pragmatic + epistemic + exploration) + risk
    }

    /// Computes a simple EFE score (legacy interface, no uncertainty).
    #[must_use]
    pub fn compute_efe_simple(expected_fans: f64, information_gain: f64) -> f64 {
        -(expected_fans + information_gain)
    }

    /// Creates an opportunity with EFE computed from the given values.
    #[must_use]
    pub fn new(
        template_id: String,
        description: String,
        expected_fans: f64,
        information_gain: f64,
        context: DispatchContext,
        reason: String,
    ) -> Self {
        Self {
            efe_score: Self::compute_efe_simple(expected_fans, information_gain),
            template_id,
            description,
            expected_fans,
            information_gain,
            context,
            reason,
        }
    }
}

/// Computes the information gain for a template given confidence and
/// prediction uncertainty.
///
/// This is a Value of Information (VoI) approximation: the expected reduction
/// in posterior entropy from one more observation. It combines:
/// - **Confidence** (observation count): more observations → less to learn.
/// - **Uncertainty** (predict_std): higher variance → more to learn.
///
/// Formula: `predict_std / sqrt(1 + confidence)`
///
/// This fixes the old `1/(1+confidence)` which ignored variance. Two
/// templates with 20 observations but very different variance now get
/// correctly different information gains.
#[must_use]
pub fn information_gain(confidence: u32, predict_std: f64) -> f64 {
    predict_std / (1.0 + confidence as f64).sqrt()
}

/// Computes softmax dispatch probabilities from EFE scores.
///
/// The brain doesn't always greedily dispatch the lowest-EFE opportunity.
/// Instead, it uses a softmax (Boltzmann) distribution: the probability of
/// dispatching opportunity `i` is proportional to `exp(-EFE_i / temperature)`.
///
/// - **Low temperature** (→0): greedy — always dispatch the best opportunity.
/// - **High temperature** (→∞): uniform — dispatch randomly (pure exploration).
/// - **Moderate temperature**: mostly exploit, sometimes explore.
#[must_use]
pub fn softmax_dispatch(efe_scores: &[f64], temperature: f64) -> Vec<f64> {
    if efe_scores.is_empty() {
        return Vec::new();
    }
    if efe_scores.len() == 1 {
        return vec![1.0];
    }
    if temperature <= 0.0 || !temperature.is_finite() {
        // Greedy: pick the minimum EFE.
        let min_idx = efe_scores
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
            .unwrap_or(0);
        let mut probs = vec![0.0; efe_scores.len()];
        probs[min_idx] = 1.0;
        return probs;
    }
    // Numerical stability: subtract the max (min EFE = max -EFE).
    let max_neg_efe = efe_scores
        .iter()
        .map(|&e| -e / temperature)
        .fold(f64::NEG_INFINITY, f64::max);
    let exps: Vec<f64> = efe_scores
        .iter()
        .map(|&e| (-e / temperature - max_neg_efe).exp())
        .collect();
    let sum: f64 = exps.iter().sum();
    if sum <= 0.0 || !sum.is_finite() {
        return vec![1.0 / efe_scores.len() as f64; efe_scores.len()];
    }
    exps.iter().map(|&e| e / sum).collect()
}

/// Computes the adaptive exploration temperature from regret.
///
/// High regret → the brain is missing opportunities → increase exploration.
/// Low regret → the brain is doing well → decrease exploration (exploit).
#[must_use]
pub fn adaptive_temperature(regret: f64, min_temp: f64, max_temp: f64) -> f64 {
    let sigmoid = 1.0 / (1.0 + (-regret).exp());
    min_temp + (max_temp - min_temp) * sigmoid
}

/// Convenience: compute EFE with default weights.
#[must_use]
pub fn compute_efe(
    expected_fans: f64,
    information_gain: f64,
    predict_std: f64,
    novelty: f64,
) -> f64 {
    GrowthOpportunity::compute_efe(
        expected_fans,
        information_gain,
        predict_std,
        novelty,
        EfeWeights::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn efe_score_balances_fans_and_information() {
        let weights = EfeWeights::default();
        let high_fans = GrowthOpportunity::compute_efe(10.0, 0.5, 1.0, 0.5, weights);
        let low_fans = GrowthOpportunity::compute_efe(2.0, 0.5, 1.0, 0.5, weights);
        assert!(high_fans < low_fans);
        let high_info = GrowthOpportunity::compute_efe(5.0, 1.0, 1.0, 0.5, weights);
        let low_info = GrowthOpportunity::compute_efe(5.0, 0.1, 1.0, 0.5, weights);
        assert!(high_info < low_info);
        let high_uncertainty = GrowthOpportunity::compute_efe(5.0, 0.5, 3.0, 0.5, weights);
        let low_uncertainty = GrowthOpportunity::compute_efe(5.0, 0.5, 0.5, 0.5, weights);
        assert!(
            high_uncertainty < low_uncertainty,
            "higher uncertainty should lower EFE (more to learn)"
        );
        let high_novelty = GrowthOpportunity::compute_efe(5.0, 0.5, 1.0, 1.0, weights);
        let low_novelty = GrowthOpportunity::compute_efe(5.0, 0.5, 1.0, 0.1, weights);
        assert!(high_novelty < low_novelty);
    }

    #[test]
    fn information_gain_decreases_with_confidence() {
        let std = 2.0;
        assert!(information_gain(0, std) > information_gain(10, std));
        assert!(information_gain(10, std) > information_gain(50, std));
    }

    #[test]
    fn information_gain_increases_with_uncertainty() {
        // Same confidence, different uncertainty.
        let low_std = information_gain(10, 0.5);
        let high_std = information_gain(10, 5.0);
        assert!(
            high_std > low_std,
            "higher uncertainty → higher information gain"
        );
    }

    #[test]
    fn information_gain_zero_std_is_zero() {
        // No uncertainty → nothing to learn.
        assert!((information_gain(0, 0.0)).abs() < 0.001);
    }

    #[test]
    fn opportunity_new_computes_efe_automatically() {
        let opp = GrowthOpportunity::new(
            "reddit-scanner".to_owned(),
            "Scan for new communities".to_owned(),
            5.0,
            0.8,
            DispatchContext::default(),
            "Stagnant growth".to_owned(),
        );
        assert!((opp.efe_score - (-5.8)).abs() < 0.01);
    }

    #[test]
    fn efe_risk_penalty_makes_uncertain_opportunities_less_attractive() {
        let weights = EfeWeights::default();
        let certain = GrowthOpportunity::compute_efe(5.0, 0.5, 0.5, 0.5, weights);
        let uncertain = GrowthOpportunity::compute_efe(5.0, 0.5, 5.0, 0.5, weights);
        assert!(
            uncertain < certain,
            "at default weights, brain should prefer uncertain opportunities (epistemic > risk)"
        );
    }

    #[test]
    fn efe_risk_averse_weights_prefer_certain_opportunities() {
        let weights = EfeWeights {
            pragmatic: 1.0,
            epistemic: 0.1,
            exploration: 0.3,
            risk: 1.0,
        };
        let certain = GrowthOpportunity::compute_efe(5.0, 0.5, 0.5, 0.5, weights);
        let uncertain = GrowthOpportunity::compute_efe(5.0, 0.5, 5.0, 0.5, weights);
        assert!(
            certain < uncertain,
            "with high risk aversion, brain should prefer certain opportunities"
        );
    }

    // ── Softmax dispatch tests ──

    #[test]
    fn softmax_dispatch_empty_returns_empty() {
        let probs = softmax_dispatch(&[], 1.0);
        assert!(probs.is_empty());
    }

    #[test]
    fn softmax_dispatch_single_returns_one() {
        let probs = softmax_dispatch(&[-5.0], 1.0);
        assert_eq!(probs, vec![1.0]);
    }

    #[test]
    fn softmax_dispatch_lower_efe_gets_higher_probability() {
        let probs = softmax_dispatch(&[-10.0, -2.0], 1.0);
        assert!(probs[0] > probs[1]);
    }

    #[test]
    fn softmax_dispatch_probabilities_sum_to_one() {
        let probs = softmax_dispatch(&[-5.0, -3.0, -8.0, -1.0], 0.5);
        let sum: f64 = probs.iter().sum();
        assert!((sum - 1.0).abs() < 0.001);
    }

    #[test]
    fn softmax_dispatch_low_temperature_is_greedy() {
        let probs = softmax_dispatch(&[-10.0, -9.0], 0.01);
        assert!(probs[0] > 0.99);
    }

    #[test]
    fn softmax_dispatch_high_temperature_is_uniform() {
        let probs = softmax_dispatch(&[-10.0, -2.0], 1000.0);
        assert!((probs[0] - 0.5).abs() < 0.01);
    }

    #[test]
    fn softmax_dispatch_handles_extreme_values() {
        let probs = softmax_dispatch(&[-1e10, -1.0, 0.0], 1.0);
        assert!(probs.iter().all(|p| p.is_finite() && *p >= 0.0));
        let sum: f64 = probs.iter().sum();
        assert!((sum - 1.0).abs() < 0.001);
    }

    #[test]
    fn softmax_dispatch_zero_temperature_is_greedy() {
        let probs = softmax_dispatch(&[-10.0, -2.0, -5.0], 0.0);
        assert_eq!(probs[0], 1.0);
        assert_eq!(probs[1], 0.0);
    }

    // ── Adaptive temperature tests ──

    #[test]
    fn adaptive_temperature_zero_regret_is_moderate() {
        let temp = adaptive_temperature(0.0, 0.1, 2.0);
        assert!(temp > 0.1 && temp < 2.0);
    }

    #[test]
    fn adaptive_temperature_high_regret_approaches_max() {
        let temp = adaptive_temperature(10.0, 0.1, 2.0);
        assert!(temp > 1.9);
    }

    #[test]
    fn adaptive_temperature_negative_regret_approaches_min() {
        let temp = adaptive_temperature(-10.0, 0.1, 2.0);
        assert!(temp < 0.2);
    }

    #[test]
    fn adaptive_temperature_is_bounded() {
        let temp_high = adaptive_temperature(1000.0, 0.1, 2.0);
        let temp_low = adaptive_temperature(-1000.0, 0.1, 2.0);
        assert!(temp_high <= 2.0);
        assert!(temp_low >= 0.1);
    }
}
