//! Value of Information (VoI) and option value computations.
//!
//! The brain uses VoI to decide whether gathering more information is worth
//! the cost of waiting. Every dispatch opportunity has an option value — the
//! value of keeping it open rather than committing now — and an expected
//! information gain from each additional observation. Together these feed the
//! EFE scorer's epistemic term.

use serde::Serialize;

/// A snapshot of the VoI and option-value metrics for a dispatch opportunity.
///
/// This is serialized alongside the opportunity so the brain's reasoning is
/// auditable: you can see *why* it chose to wait or to commit.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct VoiAssessment {
    /// The Value of Information from one more observation.
    pub voi: f64,
    /// The real option value of keeping the opportunity open.
    pub option_value: f64,
    /// The expected reduction in variance from one observation.
    pub expected_information_gain: f64,
    /// The combined exploration bonus (novelty × VoI × weight).
    pub exploration_bonus: f64,
}

/// Computes the Value of Information (VoI) approximation.
///
/// VoI is the expected reduction in posterior entropy from one more
/// observation. Higher uncertainty and fewer observations mean there is more
/// to learn, so the value of gathering information is greater.
///
/// Formula: `posterior_std / sqrt(1 + n)`
///
/// - `posterior_std` — the current posterior standard deviation.
/// - `n` — the number of observations already collected.
#[must_use]
pub fn value_of_information(posterior_std: f64, n: u32) -> f64 {
    posterior_std / (1.0 + n as f64).sqrt()
}

/// Computes the real option value of keeping a dispatch opportunity open.
///
/// This is the value of *not* committing now — of preserving the flexibility
/// to dispatch later when more information is available. It uses a simplified
/// Black-Scholes-like approximation: the spread between best and worst case,
/// scaled by uncertainty and discounted by the time cost of waiting.
///
/// Formula: `discount * (best_case - worst_case) * uncertainty / (1.0 + uncertainty)`
///
/// - `uncertainty` — the current uncertainty about the outcome (e.g. posterior
///   std).
/// - `best_case` — the expected value in the best plausible scenario.
/// - `worst_case` — the expected value in the worst plausible scenario.
/// - `discount` — the time-discount factor for waiting (0–1).
#[must_use]
pub fn option_value(uncertainty: f64, best_case: f64, worst_case: f64, discount: f64) -> f64 {
    discount * (best_case - worst_case) * uncertainty / (1.0 + uncertainty)
}

/// Computes the expected information gain from one observation.
///
/// This is the expected reduction in uncertainty (variance) from a single
/// additional observation. It is the difference between the prior variance
/// (before the observation) and the posterior variance (after).
///
/// Formula: `prior_variance - posterior_variance`
///
/// - `prior_variance` — the variance before the observation.
/// - `posterior_variance` — the expected variance after the observation.
#[must_use]
pub fn expected_information_gain(prior_variance: f64, posterior_variance: f64) -> f64 {
    prior_variance - posterior_variance
}

/// Computes the combined exploration bonus for the EFE scorer.
///
/// The exploration bonus rewards the brain for exploring novel territory with
/// high information value. It is the product of novelty, VoI, and a
/// configurable exploration weight, so well-explored or low-information
/// opportunities receive little bonus.
///
/// Formula: `exploration_weight * novelty * voi`
///
/// - `novelty` — the novelty score from the exploration memory (0–1).
/// - `voi` — the Value of Information from [`value_of_information`].
/// - `exploration_weight` — how strongly the brain values exploration.
#[must_use]
pub fn exploration_bonus(novelty: f64, voi: f64, exploration_weight: f64) -> f64 {
    exploration_weight * novelty * voi
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── value_of_information ──

    #[test]
    fn voi_decreases_with_more_observations() {
        let std = 2.0;
        let voi_0 = value_of_information(std, 0);
        let voi_10 = value_of_information(std, 10);
        let voi_100 = value_of_information(std, 100);
        assert!(voi_0 > voi_10);
        assert!(voi_10 > voi_100);
    }

    #[test]
    fn voi_increases_with_uncertainty() {
        let n = 10;
        let low_std = value_of_information(0.5, n);
        let high_std = value_of_information(5.0, n);
        assert!(
            high_std > low_std,
            "higher uncertainty → higher value of information"
        );
    }

    #[test]
    fn voi_zero_std_is_zero() {
        // No uncertainty → nothing to learn.
        assert!((value_of_information(0.0, 0)).abs() < 0.001);
        assert!((value_of_information(0.0, 100)).abs() < 0.001);
    }

    #[test]
    fn voi_zero_observations_uses_full_std() {
        // With n=0, denominator is sqrt(1) = 1, so VoI equals the std.
        let std = 3.0;
        let voi = value_of_information(std, 0);
        assert!((voi - std).abs() < 0.001);
    }

    #[test]
    fn voi_is_always_non_negative_for_non_negative_std() {
        for n in [0_u32, 1, 10, 100, 1000] {
            assert!(value_of_information(2.0, n) >= 0.0);
        }
    }

    // ── option_value ──

    #[test]
    fn option_value_scales_with_spread() {
        let uncertainty = 1.0;
        let discount = 1.0;
        let narrow = option_value(uncertainty, 10.0, 8.0, discount);
        let wide = option_value(uncertainty, 10.0, 2.0, discount);
        assert!(
            wide > narrow,
            "wider spread between best and worst case → higher option value"
        );
    }

    #[test]
    fn option_value_scales_with_uncertainty() {
        let best = 10.0;
        let worst = 0.0;
        let discount = 1.0;
        let low_u = option_value(0.1, best, worst, discount);
        let high_u = option_value(5.0, best, worst, discount);
        assert!(high_u > low_u, "higher uncertainty → higher option value");
    }

    #[test]
    fn option_value_discounted_by_waiting() {
        let uncertainty = 1.0;
        let best = 10.0;
        let worst = 0.0;
        let now = option_value(uncertainty, best, worst, 1.0);
        let later = option_value(uncertainty, best, worst, 0.5);
        assert!(
            now > later,
            "less discounting (commit sooner) → higher option value"
        );
    }

    #[test]
    fn option_value_zero_uncertainty_is_zero() {
        // No uncertainty → no value in waiting.
        assert!((option_value(0.0, 10.0, 0.0, 1.0)).abs() < 0.001);
    }

    #[test]
    fn option_value_zero_spread_is_zero() {
        // best_case == worst_case → no value in flexibility.
        assert!((option_value(1.0, 5.0, 5.0, 1.0)).abs() < 0.001);
    }

    #[test]
    fn option_value_zero_discount_is_zero() {
        // No discount (wait forever) → zero present value.
        assert!((option_value(1.0, 10.0, 0.0, 0.0)).abs() < 0.001);
    }

    #[test]
    fn option_value_decreases_then_saturates_with_uncertainty() {
        // As uncertainty → ∞, the factor u/(1+u) → 1, so option value
        // saturates at discount * (best - worst). It should never exceed
        // that ceiling.
        let best = 10.0;
        let worst = 0.0;
        let discount = 0.8;
        let ceiling = discount * (best - worst);
        let very_high = option_value(1e6, best, worst, discount);
        assert!(
            very_high <= ceiling + 1e-6,
            "option value must not exceed the discounted spread ceiling"
        );
    }

    // ── expected_information_gain ──

    #[test]
    fn eig_is_variance_reduction() {
        let gain = expected_information_gain(10.0, 4.0);
        assert!((gain - 6.0).abs() < 0.001);
    }

    #[test]
    fn eig_zero_when_no_reduction() {
        // Prior == posterior → nothing learned.
        assert!((expected_information_gain(5.0, 5.0)).abs() < 0.001);
    }

    #[test]
    fn eig_zero_variance_is_zero() {
        // No prior variance → nothing to reduce.
        assert!((expected_information_gain(0.0, 0.0)).abs() < 0.001);
    }

    #[test]
    fn eig_negative_when_variance_increases() {
        // If posterior variance is higher than prior, the "gain" is negative.
        let gain = expected_information_gain(2.0, 5.0);
        assert!(gain < 0.0);
    }

    // ── exploration_bonus ──

    #[test]
    fn exploration_bonus_scales_with_novelty() {
        let voi = 1.0;
        let weight = 1.0;
        let low_novelty = exploration_bonus(0.1, voi, weight);
        let high_novelty = exploration_bonus(0.9, voi, weight);
        assert!(high_novelty > low_novelty);
    }

    #[test]
    fn exploration_bonus_scales_with_voi() {
        let novelty = 0.5;
        let weight = 1.0;
        let low_voi = exploration_bonus(novelty, 0.1, weight);
        let high_voi = exploration_bonus(novelty, 5.0, weight);
        assert!(high_voi > low_voi);
    }

    #[test]
    fn exploration_bonus_scales_with_weight() {
        let novelty = 0.5;
        let voi = 1.0;
        let low_w = exploration_bonus(novelty, voi, 0.1);
        let high_w = exploration_bonus(novelty, voi, 2.0);
        assert!(high_w > low_w);
    }

    #[test]
    fn exploration_bonus_zero_novelty_is_zero() {
        // Fully explored territory → no exploration bonus.
        assert!((exploration_bonus(0.0, 5.0, 1.0)).abs() < 0.001);
    }

    #[test]
    fn exploration_bonus_zero_voi_is_zero() {
        // No information to gain → no exploration bonus.
        assert!((exploration_bonus(0.9, 0.0, 1.0)).abs() < 0.001);
    }

    #[test]
    fn exploration_bonus_zero_weight_is_zero() {
        // Exploration disabled → no bonus.
        assert!((exploration_bonus(0.9, 5.0, 0.0)).abs() < 0.001);
    }

    // ── VoiAssessment serialization ──

    #[test]
    fn voi_assessment_serializes() {
        let assessment = VoiAssessment {
            voi: 1.5,
            option_value: 3.0,
            expected_information_gain: 2.0,
            exploration_bonus: 0.75,
        };
        let json = serde_json::to_string(&assessment).expect("serialize");
        assert!(json.contains("\"voi\":1.5"));
        assert!(json.contains("\"option_value\":3.0"));
        assert!(json.contains("\"expected_information_gain\":2.0"));
        assert!(json.contains("\"exploration_bonus\":0.75"));
    }

    #[test]
    fn voi_assessment_default_is_all_zero() {
        let assessment = VoiAssessment::default();
        assert!((assessment.voi).abs() < 0.001);
        assert!((assessment.option_value).abs() < 0.001);
        assert!((assessment.expected_information_gain).abs() < 0.001);
        assert!((assessment.exploration_bonus).abs() < 0.001);
    }
}
