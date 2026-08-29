//! Value of Information (VoI) and option value computations.
//!
//! The brain uses VoI to decide whether gathering more information is worth
//! the cost of waiting. Every dispatch opportunity has an option value — the
//! value of keeping it open rather than committing now — and an expected
//! information gain from each additional observation. Together these feed the
//! EFE scorer's epistemic term.
//!
//! # Knowledge Gradient (KG)
//!
//! The Knowledge Gradient is a principled VoI measure for Bayesian learning.
//! Unlike the heuristic [`value_of_information`], the KG computes the *exact*
//! expected improvement in the best decision after one more observation.
//!
//! For a Normal posterior with mean μ and variance σ², observing one sample
//! with measurement variance σ_obs² updates the posterior to:
//!
//! ```text
//! σ_post² = 1 / (1/σ² + 1/σ_obs²)
//! ```
//!
//! The KG is the expected increase in the maximum of the posterior:
//!
//! ```text
//! KG = E[max(μ_post, 0)] - max(μ, 0)
//! ```
//!
//! For a single alternative (should we dispatch or not?), the KG tells us
//! exactly how much better our best decision will be after one observation.
//! This is the "true VoI" — it accounts for both the uncertainty reduction
//! *and* how that reduction changes the optimal decision.

use serde::Serialize;

use crate::bayesian::{normal_cdf, normal_pdf};

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
/// This is the same computation as [`crate::efe::information_gain`],
/// exposed here under the VoI name for callers that reason in terms of
/// "value of information" rather than "information gain".
///
/// Formula: `posterior_std / sqrt(1 + n)`
///
/// - `posterior_std` — the current posterior standard deviation.
/// - `n` — the number of observations already collected.
#[must_use]
pub fn value_of_information(posterior_std: f64, n: u32) -> f64 {
    crate::efe::information_gain(n, posterior_std)
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

// ─── Knowledge Gradient ─────────────────────────────────────────────────────

/// Computes the Knowledge Gradient (KG) for a single-alternative Bayesian
/// decision problem.
///
/// The KG is the expected improvement in the best decision after one more
/// observation. For the binary decision "dispatch or not" (value vs. zero),
/// the KG is:
///
/// ```text
/// KG = E[max(μ_post, 0)] - max(μ, 0)
/// ```
///
/// where μ_post is the posterior mean after the observation. Since the
/// posterior mean is a random variable (it depends on the observation), we
/// compute its expectation analytically using the Normal-Normal conjugate
/// model.
///
/// # Arguments
///
/// - `prior_mean` (μ) — the current posterior mean.
/// - `prior_variance` (σ²) — the current posterior variance.
/// - `observation_variance` (σ_obs²) — the variance of one observation.
///
/// # Returns
///
/// The expected improvement in `max(·, 0)` from one observation. Always
/// non-negative — more information can never make the best decision worse.
///
/// # Mathematical details
///
/// After one observation, the posterior variance becomes:
///
/// ```text
/// σ_post² = 1 / (1/σ² + 1/σ_obs²) = σ² · σ_obs² / (σ² + σ_obs²)
/// ```
///
/// The posterior mean is a Normal random variable with mean μ and variance
/// equal to the "learning variance":
///
/// ```text
/// σ_learn² = σ² - σ_post² = σ⁴ / (σ² + σ_obs²)
/// ```
///
/// The KG for `max(·, 0)` with a Normal variable X ~ N(μ, σ_learn²) is:
///
/// ```text
/// E[max(X, 0)] = μ · Φ(μ/σ_learn) + σ_learn · φ(μ/σ_learn)
/// ```
///
/// where Φ is the Normal CDF and φ is the Normal PDF. The KG is this minus
/// `max(μ, 0)`.
#[must_use]
pub fn knowledge_gradient(prior_mean: f64, prior_variance: f64, observation_variance: f64) -> f64 {
    // Degenerate cases: no prior uncertainty or no observation noise → no learning.
    if prior_variance <= 0.0 || observation_variance <= 0.0 {
        return 0.0;
    }

    // Posterior variance after one observation (Normal-Normal conjugate).
    let post_variance =
        prior_variance * observation_variance / (prior_variance + observation_variance);

    // Learning variance: how much the posterior mean can move.
    let learn_variance = prior_variance - post_variance;
    if learn_variance <= 0.0 {
        return 0.0;
    }
    let learn_std = learn_variance.sqrt();

    // E[max(X, 0)] where X ~ N(prior_mean, learn_variance).
    // = prior_mean * Φ(prior_mean / learn_std) + learn_std * φ(prior_mean / learn_std)
    let z = prior_mean / learn_std;
    let expected_max_post = prior_mean * normal_cdf(z) + learn_std * normal_pdf(z);

    // Current best: max(prior_mean, 0).
    let current_best = prior_mean.max(0.0);

    // KG = E[max(μ_post, 0)] - max(μ, 0). Always >= 0.
    (expected_max_post - current_best).max(0.0)
}

/// Portfolio-level Knowledge Gradient (P2.2).
///
/// Computes the expected improvement in the portfolio's total expected fans
/// after observing one more outcome for the candidate at `candidate_idx`.
///
/// Unlike the single-alternative [`knowledge_gradient`], this accounts for
/// the portfolio structure: the candidate competes with other candidates for
/// limited dispatch slots. The KG is the expected increase in the total
/// portfolio value after learning the true value of this candidate.
///
/// # Algorithm
///
/// 1. Simulate the post-observation posterior for the candidate (Normal-Normal
///    conjugate: the posterior mean is a Normal random variable).
/// 2. For each possible posterior mean (discretized), re-rank the candidates
///    and recompute the portfolio value.
/// 3. The KG is the expected portfolio improvement, weighted by the
///    probability of each posterior mean.
///
/// # Arguments
///
/// * `means` — posterior means for all candidates.
/// * `variances` — posterior variances for all candidates.
/// * `candidate_idx` — the index of the candidate being considered for
///   exploration.
/// * `observation_variance` — the measurement variance for the candidate.
/// * `portfolio_size` — the number of candidates the portfolio can dispatch.
///
/// # Returns
///
/// The expected improvement in the portfolio's total expected fans.
#[must_use]
pub fn portfolio_kg(
    means: &[f64],
    variances: &[f64],
    candidate_idx: usize,
    observation_variance: f64,
    portfolio_size: usize,
) -> f64 {
    if means.is_empty()
        || candidate_idx >= means.len()
        || means.len() != variances.len()
        || portfolio_size == 0
    {
        return 0.0;
    }

    let prior_var = variances[candidate_idx];
    if prior_var <= 0.0 || observation_variance <= 0.0 {
        return 0.0;
    }

    // Posterior variance after one observation.
    let post_var = prior_var * observation_variance / (prior_var + observation_variance);
    // Learning variance: how much the posterior mean can move.
    let learn_var = prior_var - post_var;
    if learn_var <= 0.0 {
        return 0.0;
    }
    let learn_std = learn_var.sqrt();

    // Current portfolio value: sum of the top `portfolio_size` means.
    let current_portfolio = top_n_sum(means, portfolio_size);

    // Discretize the posterior mean distribution and compute the expected
    // portfolio improvement. We use Gauss-Hermite quadrature with 5 points
    // for a balance of accuracy and speed.
    let prior_mean = means[candidate_idx];

    // E[portfolio_value_after_observation] - current_portfolio_value.
    // For each possible posterior mean μ', re-rank and recompute.
    // We approximate with a few representative points.
    let points = [
        (-2.0_f64, 0.053),
        (-1.0, 0.242),
        (0.0, 0.399),
        (1.0, 0.242),
        (2.0, 0.053),
    ];
    let mut expected_improvement = 0.0;
    for &(offset, weight) in &points {
        let post_mean = prior_mean + offset * learn_std;
        // Build the new means vector with the updated candidate.
        let mut new_means: Vec<f64> = means.to_vec();
        new_means[candidate_idx] = post_mean;
        let new_portfolio = top_n_sum(&new_means, portfolio_size);
        let improvement = (new_portfolio - current_portfolio).max(0.0);
        expected_improvement += weight * improvement;
    }

    // Normalize weights (they should sum to ~0.989, close to 1.0).
    expected_improvement / 0.989
}

/// Returns the sum of the top `n` values in a slice.
fn top_n_sum(values: &[f64], n: usize) -> f64 {
    if values.is_empty() || n == 0 {
        return 0.0;
    }
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    sorted.iter().take(n).sum()
}

/// Computes the Knowledge Gradient for a multi-alternative ranking problem.
///
/// When the brain is choosing between multiple templates (alternatives) and
/// wants to know which one to experiment with, the KG for each alternative
/// is computed independently and the one with the highest KG is selected.
///
/// This function computes the KG for each alternative and returns the index
/// of the best one to experiment with, along with its KG value.
///
/// # Arguments
///
/// - `means` — the posterior means for each alternative.
/// - `variances` — the posterior variances for each alternative.
/// - `observation_variance` — the variance of one observation (same for all).
///
/// # Returns
///
/// `(best_index, best_kg)` — the index of the alternative with the highest KG
/// and its KG value. Returns `(0, 0.0)` if the input is empty.
#[must_use]
pub fn knowledge_gradient_ranking(
    means: &[f64],
    variances: &[f64],
    observation_variance: f64,
) -> (usize, f64) {
    if means.is_empty() || variances.is_empty() || means.len() != variances.len() {
        return (0, 0.0);
    }

    means
        .iter()
        .zip(variances.iter())
        .map(|(&m, &v)| knowledge_gradient(m, v, observation_variance))
        .enumerate()
        .max_by(|(_, kg_a), (_, kg_b)| kg_a.partial_cmp(kg_b).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or((0, 0.0))
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

    // ─── Knowledge Gradient tests ─────────────────────────────────────────

    #[test]
    fn kg_zero_prior_variance_is_zero() {
        // No uncertainty → no value in learning.
        let kg = knowledge_gradient(5.0, 0.0, 4.0);
        assert!(
            (kg).abs() < 0.001,
            "zero prior variance → zero KG, got {kg}"
        );
    }

    #[test]
    fn kg_zero_observation_variance_is_zero() {
        // Perfect observation → no posterior change → no KG.
        let kg = knowledge_gradient(5.0, 4.0, 0.0);
        assert!(
            (kg).abs() < 0.001,
            "zero observation variance → zero KG, got {kg}"
        );
    }

    #[test]
    fn kg_is_always_non_negative() {
        // KG should never be negative — more info can't hurt.
        for &mean in &[0.0, 5.0, -5.0, 10.0, -10.0] {
            for &pv in &[1.0, 4.0, 16.0] {
                for &ov in &[1.0, 4.0, 16.0] {
                    let kg = knowledge_gradient(mean, pv, ov);
                    assert!(
                        kg >= 0.0,
                        "KG should be non-negative: mean={mean}, pv={pv}, ov={ov}, kg={kg}"
                    );
                }
            }
        }
    }

    #[test]
    fn kg_increases_with_prior_uncertainty() {
        // More prior uncertainty → more to learn → higher KG.
        let kg_low = knowledge_gradient(0.0, 1.0, 4.0);
        let kg_high = knowledge_gradient(0.0, 16.0, 4.0);
        assert!(
            kg_high > kg_low,
            "higher prior variance → higher KG: low={kg_low}, high={kg_high}"
        );
    }

    #[test]
    fn kg_decreases_with_observation_noise() {
        // More observation noise → less learning → lower KG.
        let kg_low_noise = knowledge_gradient(0.0, 4.0, 1.0);
        let kg_high_noise = knowledge_gradient(0.0, 4.0, 16.0);
        assert!(
            kg_low_noise > kg_high_noise,
            "lower observation noise → higher KG: low_noise={kg_low_noise}, high_noise={kg_high_noise}"
        );
    }

    #[test]
    fn kg_positive_mean_near_zero_is_high() {
        // When the mean is near zero, we're uncertain about the sign of the
        // effect — one observation could flip the decision. This is the
        // highest-KG scenario.
        let kg_near_zero = knowledge_gradient(0.0, 4.0, 4.0);
        let kg_positive = knowledge_gradient(10.0, 4.0, 4.0);
        assert!(
            kg_near_zero > kg_positive,
            "mean near zero → higher KG than confident positive: near_zero={kg_near_zero}, positive={kg_positive}"
        );
    }

    #[test]
    fn kg_very_confident_positive_is_low() {
        // When the mean is very positive relative to uncertainty, we already
        // know the decision — KG should be small.
        let kg = knowledge_gradient(100.0, 1.0, 4.0);
        assert!(kg < 0.1, "very confident positive → low KG, got {kg}");
    }

    #[test]
    fn kg_very_confident_negative_is_low() {
        // When the mean is very negative relative to uncertainty, we already
        // know not to dispatch — KG should be small.
        let kg = knowledge_gradient(-100.0, 1.0, 4.0);
        assert!(kg < 0.1, "very confident negative → low KG, got {kg}");
    }

    #[test]
    fn kg_symmetric_around_zero_mean() {
        // KG(μ) should equal KG(-μ) for the same variances, because the
        // decision boundary is at zero.
        let kg_pos = knowledge_gradient(5.0, 4.0, 4.0);
        let kg_neg = knowledge_gradient(-5.0, 4.0, 4.0);
        assert!(
            (kg_pos - kg_neg).abs() < 0.001,
            "KG should be symmetric around zero: pos={kg_pos}, neg={kg_neg}"
        );
    }

    #[test]
    fn kg_ranking_picks_highest_kg() {
        let means = vec![10.0, 0.0, -10.0];
        let variances = vec![1.0, 4.0, 1.0];
        let (idx, kg) = knowledge_gradient_ranking(&means, &variances, 4.0);
        // Alternative 1 (mean=0, var=4) has the highest KG because it's
        // near the decision boundary with high uncertainty.
        assert_eq!(idx, 1);
        assert!(kg > 0.0);
    }

    #[test]
    fn kg_ranking_empty_returns_zero() {
        let (idx, kg) = knowledge_gradient_ranking(&[], &[], 4.0);
        assert_eq!(idx, 0);
        assert!((kg).abs() < 0.001);
    }

    #[test]
    fn kg_ranking_mismatched_lengths_returns_zero() {
        let means = vec![1.0, 2.0];
        let variances = vec![1.0];
        let (idx, kg) = knowledge_gradient_ranking(&means, &variances, 4.0);
        assert_eq!(idx, 0);
        assert!((kg).abs() < 0.001);
    }

    #[test]
    fn kg_ranking_all_zero_variance_returns_zero_kg() {
        let means = vec![5.0, 10.0];
        let variances = vec![0.0, 0.0];
        let (_, kg) = knowledge_gradient_ranking(&means, &variances, 4.0);
        assert!((kg).abs() < 0.001);
    }
}
