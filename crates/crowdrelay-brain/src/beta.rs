//! Beta-Bernoulli conjugate posteriors for binary outcomes.
//!
//! Binary outcomes (reply / no-reply, conversion / no-conversion) need a
//! different conjugate family than the Normal-Normal (treatment effects) or
//! Gamma-Poisson (fan counts). The Beta-Bernoulli is the correct model: the
//! success probability `p` has a Beta prior, and each observation is a
//! Bernoulli trial.
//!
//! See `bayesian.rs` for the Normal and NegBin posteriors. This module is
//! split out to keep `bayesian.rs` under the source-size ratchet.

use serde::{Deserialize, Serialize};

/// A Beta-Bernoulli conjugate posterior for binary outcomes (reply / no-reply,
/// conversion / no-conversion, placement / no-placement).
///
/// Binary outcomes are the natural model for outreach reply prediction: a
/// pitch either gets a positive reply or it does not. The Normal-Normal model
/// is wrong here because it assumes continuous Gaussian observations; the
/// Gamma-Poisson model is wrong because it models non-negative integer counts,
/// not a single Bernoulli trial. The Beta-Bernoulli is the correct conjugate
/// family.
///
/// # Model
///
/// The success probability `p` has a Beta prior:
///
/// ```text
/// p ~ Beta(α, β)              (prior)
/// y ~ Bernoulli(p)            (observation, y ∈ {0, 1})
/// p | y ~ Beta(α + y, β + 1 − y)   (posterior)
/// ```
///
/// For multiple observations y₁..yₙ:
///
/// ```text
/// p | y₁..yₙ ~ Beta(α + Σyᵢ, β + n − Σyᵢ)
/// ```
///
/// The posterior mean is `α / (α + β)`, which is the brain's best estimate of
/// the success probability. The variance is `αβ / ((α+β)² (α+β+1))`, which
/// shrinks as observations accumulate.
///
/// # Priors
///
/// `Beta(1, 1)` is the uniform prior — every probability is equally likely
/// before any observation. For outreach, an informative prior such as
/// `Beta(1, 9)` encodes "we expect about 10% positive replies before we have
/// data", which is a conservative starting point that fails closed: a target
/// with no observations predicts the base rate, not a coin flip.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BetaPosterior {
    /// Posterior shape α (successes + prior α).
    pub alpha: f64,
    /// Posterior shape β (failures + prior β).
    pub beta: f64,
    /// Number of observations.
    pub n: u32,
}

impl Default for BetaPosterior {
    fn default() -> Self {
        // Beta(1, 1) — uniform prior. The reply model overrides this with an
        // informative prior encoding the expected base rate.
        Self::prior(1.0, 1.0)
    }
}

impl BetaPosterior {
    /// Creates a Beta prior with the given shapes.
    ///
    /// `alpha` and `beta` must be positive. The prior mean is `alpha / (alpha +
    /// beta)`. Use `from_mean(mean, pseudo_observations)` for a prior
    /// encoding an expected base rate with a known strength.
    #[must_use]
    pub const fn prior(alpha: f64, beta: f64) -> Self {
        Self { alpha, beta, n: 0 }
    }

    /// Creates a Beta prior encoding an expected success probability with a
    /// given pseudo-observation strength.
    ///
    /// `mean` is the expected success probability (0, 1). `pseudo_observations`
    /// is the prior strength — how many observations worth of evidence the
    /// prior represents. `Beta(1, 9)` (a 10% base rate) corresponds to
    /// `from_mean(0.1, 10.0)`.
    #[must_use]
    pub fn from_mean(mean: f64, pseudo_observations: f64) -> Self {
        let m = mean.clamp(0.001, 0.999);
        let n = pseudo_observations.max(1.0);
        Self {
            alpha: m * n,
            beta: (1.0 - m) * n,
            n: 0,
        }
    }

    /// Bayesian update with one binary observation.
    ///
    /// Beta-Bernoulli conjugate: `α += y, β += 1 − y` where `y ∈ {0, 1}`.
    pub fn update(&mut self, success: bool) {
        if success {
            self.alpha += 1.0;
        } else {
            self.beta += 1.0;
        }
        self.n += 1;
    }

    /// Posterior mean — the brain's best estimate of the success probability.
    ///
    /// `E[p] = α / (α + β)`. This is the point estimate the ranking uses.
    #[must_use]
    pub fn mean(&self) -> f64 {
        let sum = self.alpha + self.beta;
        if sum <= 0.0 {
            0.5
        } else {
            (self.alpha / sum).clamp(0.0, 1.0)
        }
    }

    /// Posterior variance — the brain's uncertainty about the success
    /// probability.
    ///
    /// `Var[p] = αβ / ((α+β)² (α+β+1))`. Shrinks as observations accumulate.
    #[must_use]
    pub fn variance(&self) -> f64 {
        let sum = self.alpha + self.beta;
        if sum <= 0.0 || sum + 1.0 <= 0.0 {
            return 0.25;
        }
        (self.alpha * self.beta) / (sum * sum * (sum + 1.0))
    }

    /// Posterior standard deviation.
    #[must_use]
    pub fn std(&self) -> f64 {
        self.variance().max(0.0).sqrt()
    }

    /// 95% credible interval for the success probability, using the normal
    /// approximation (valid when `α + β` is large enough, roughly > 30).
    /// For small samples this is a rough guide, not a precise interval.
    #[must_use]
    pub fn ci_95(&self) -> (f64, f64) {
        let z = 1.96;
        let std = self.std();
        let mean = self.mean();
        (mean - z * std, mean + z * std)
    }

    /// Returns the number of observations.
    #[must_use]
    pub fn confidence(&self) -> u32 {
        self.n
    }
}

/// A hierarchical Beta posterior with partial pooling across grouping levels
/// — the binary-outcome analogue of `HierarchicalNegBinPosterior`.
///
/// The brain learns reply probabilities at multiple levels:
/// - **Global**: the base reply rate across all outreach.
/// - **By kind**: per-target-kind (playlist, radio, press, ...).
/// - **By target**: per-specific-target (one curator, one station, ...).
///
/// When a target has few observations, its posterior shrinks toward the
/// kind-level posterior. When a kind has few observations, it shrinks toward
/// the global posterior. This is hierarchical Bayesian partial pooling — the
/// gold standard for multi-level learning with unequal sample sizes, and
/// exactly what the existing `HierarchicalPosterior` and
/// `HierarchicalNegBinPosterior` do for Normal and NegBin models.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HierarchicalBetaPosterior {
    /// The global posterior — pooled across all observations.
    pub global: BetaPosterior,
    /// Per-kind posteriors.
    pub by_kind: std::collections::HashMap<String, BetaPosterior>,
    /// Per-target posteriors — one specific curator, station, publication.
    #[serde(default)]
    pub by_target: std::collections::HashMap<String, BetaPosterior>,
}

impl HierarchicalBetaPosterior {
    /// Creates a hierarchical posterior with the given global prior.
    #[must_use]
    pub fn new(global_prior: BetaPosterior) -> Self {
        Self {
            global: global_prior,
            by_kind: std::collections::HashMap::new(),
            by_target: std::collections::HashMap::new(),
        }
    }

    /// Updates the hierarchy with one binary observation.
    ///
    /// The observation updates the global posterior, the kind-level posterior
    /// (if provided), and the target-level posterior (if provided). Each
    /// child's prior is the **pre-update** global posterior — this avoids
    /// double-counting, where the observation would participate in the parent
    /// prior (via the global update) AND be applied to the child.
    pub fn update(&mut self, kind: Option<&str>, target_key: Option<&str>, success: bool) {
        // Snapshot the global posterior BEFORE updating it. The child's prior
        // must be the pre-update global — otherwise the observation is counted
        // once in the global (becoming the child's prior) and again in the
        // child's update, giving early observations extra weight.
        let global_alpha = self.global.alpha;
        let global_beta = self.global.beta;
        let (target_alpha, target_beta) = kind
            .and_then(|k| self.by_kind.get(k))
            .map_or((global_alpha, global_beta), |p| (p.alpha, p.beta));

        // Update global posterior.
        self.global.update(success);

        // Update kind posterior, using the PRE-update global as prior.
        if let Some(k) = kind {
            let prior = || BetaPosterior {
                alpha: global_alpha,
                beta: global_beta,
                n: 0,
            };
            let entry = self.by_kind.entry(k.to_owned()).or_insert_with(prior);
            entry.update(success);
        }

        // Update target posterior, using the PRE-update kind as prior.
        if let Some(tk) = target_key {
            let prior = || BetaPosterior {
                alpha: target_alpha,
                beta: target_beta,
                n: 0,
            };
            let entry = self.by_target.entry(tk.to_owned()).or_insert_with(prior);
            entry.update(success);
        }
    }

    /// Predicts the expected success probability for a kind, using partial
    /// pooling with the same shrinkage formula as the other hierarchical
    /// posteriors.
    ///
    /// Returns `(probability, variance)`. When the kind has many observations,
    /// its posterior stands on its own. When it has few, it shrinks toward
    /// the global posterior.
    #[must_use]
    pub fn predict(&self, kind: Option<&str>) -> (f64, f64) {
        const SHRINKAGE_STRENGTH: f64 = 5.0;

        let kind_post = kind.and_then(|k| self.by_kind.get(k));
        let parent_mean = self.global.mean();
        let parent_var = self.global.variance();

        match kind_post {
            Some(kp) if kp.n > 0 => {
                let weight = kp.n as f64 / (kp.n as f64 + SHRINKAGE_STRENGTH);
                let mean = weight * kp.mean() + (1.0 - weight) * parent_mean;
                let variance = weight * kp.variance() + (1.0 - weight) * parent_var;
                (mean.clamp(0.0, 1.0), variance.max(0.001))
            }
            _ => (parent_mean, parent_var.max(0.001)),
        }
    }

    /// Predicts the expected success probability for a specific target,
    /// shrinking the target's own posterior toward the kind-level prediction.
    /// A target with no observations predicts exactly what
    /// [`predict`](Self::predict) predicts.
    #[must_use]
    pub fn predict_for_target(&self, kind: Option<&str>, target_key: Option<&str>) -> (f64, f64) {
        const SHRINKAGE_STRENGTH: f64 = 5.0;
        let (parent_mean, parent_var) = self.predict(kind);
        match target_key.and_then(|tk| self.by_target.get(tk)) {
            Some(tp) if tp.n > 0 => {
                let weight = tp.n as f64 / (tp.n as f64 + SHRINKAGE_STRENGTH);
                let mean = weight * tp.mean() + (1.0 - weight) * parent_mean;
                let variance = weight * tp.variance() + (1.0 - weight) * parent_var;
                (mean.clamp(0.0, 1.0), variance.max(0.001))
            }
            _ => (parent_mean, parent_var),
        }
    }

    /// Returns the observation count for one target.
    #[must_use]
    pub fn target_confidence(&self, target_key: &str) -> u32 {
        self.by_target.get(target_key).map(|p| p.n).unwrap_or(0)
    }

    /// Returns the observation count for one kind.
    #[must_use]
    pub fn kind_confidence(&self, kind: &str) -> u32 {
        self.by_kind.get(kind).map(|p| p.n).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beta_prior_uniform_has_mean_half() {
        let post = BetaPosterior::prior(1.0, 1.0);
        assert!((post.mean() - 0.5).abs() < 1e-12);
        assert_eq!(post.n, 0);
    }

    #[test]
    fn beta_from_mean_encodes_expected_base_rate() {
        // Beta(1, 9) encodes a 10% base rate with 10 pseudo-observations.
        let post = BetaPosterior::from_mean(0.1, 10.0);
        assert!((post.mean() - 0.1).abs() < 1e-12, "got {}", post.mean());
        assert!((post.alpha - 1.0).abs() < 1e-12);
        assert!((post.beta - 9.0).abs() < 1e-12);
    }

    #[test]
    fn beta_conjugate_update_three_successes_two_failures() {
        // Beta(1, 1) + 3 successes + 2 failures = Beta(4, 3).
        let mut post = BetaPosterior::prior(1.0, 1.0);
        post.update(true);
        post.update(true);
        post.update(true);
        post.update(false);
        post.update(false);
        assert!((post.alpha - 4.0).abs() < 1e-12, "alpha={}", post.alpha);
        assert!((post.beta - 3.0).abs() < 1e-12, "beta={}", post.beta);
        assert_eq!(post.n, 5);
        // Mean = 4/7 ≈ 0.571
        assert!((post.mean() - 4.0 / 7.0).abs() < 1e-12);
    }

    #[test]
    fn beta_update_moves_mean_toward_observations() {
        let mut post = BetaPosterior::from_mean(0.1, 10.0);
        // Observe mostly successes — mean should move up from the 0.1 prior.
        // Beta(1, 9) + 8 successes + 2 failures = Beta(9, 11), mean = 9/20 = 0.45.
        for _ in 0..8 {
            post.update(true);
        }
        for _ in 0..2 {
            post.update(false);
        }
        assert!(
            post.mean() > 0.3,
            "8 successes and 2 failures should move mean well above 0.1, got {}",
            post.mean()
        );
    }

    #[test]
    fn beta_variance_shrinks_with_observations() {
        let mut post = BetaPosterior::prior(1.0, 1.0);
        let initial_var = post.variance();
        for _ in 0..50 {
            post.update(true);
        }
        assert!(
            post.variance() < initial_var,
            "variance should shrink with observations, got {} < {}",
            post.variance(),
            initial_var
        );
    }

    #[test]
    fn beta_ci_contains_mean() {
        let post = BetaPosterior::from_mean(0.3, 20.0);
        let (lo, hi) = post.ci_95();
        let mean = post.mean();
        assert!(lo < mean, "lo={lo} mean={mean}");
        assert!(mean < hi, "mean={mean} hi={hi}");
    }

    #[test]
    fn beta_confidence_counts_observations() {
        let mut post = BetaPosterior::prior(1.0, 1.0);
        assert_eq!(post.confidence(), 0);
        post.update(true);
        assert_eq!(post.confidence(), 1);
        post.update(false);
        assert_eq!(post.confidence(), 2);
    }

    // ── HierarchicalBetaPosterior tests ─────────────────────────────────

    #[test]
    fn hierarchical_beta_cold_start_returns_global_prior() {
        let hier = HierarchicalBetaPosterior::new(BetaPosterior::from_mean(0.1, 10.0));
        let (prob, _) = hier.predict_for_target(Some("playlist"), Some("target:x"));
        // No data → returns global prior mean (0.1).
        assert!(
            (prob - 0.1).abs() < 1e-12,
            "cold start should return global prior, got {prob}"
        );
    }

    #[test]
    fn hierarchical_beta_kind_pools() {
        let mut hier = HierarchicalBetaPosterior::new(BetaPosterior::from_mean(0.1, 10.0));
        // Give "press" kind lots of positive replies.
        for _ in 0..20 {
            hier.update(Some("press"), None, true);
        }
        // Give "playlist" kind mostly failures, to keep the global lower.
        for _ in 0..20 {
            hier.update(Some("playlist"), None, false);
        }
        let (press_prob, _) = hier.predict(Some("press"));
        let (playlist_prob, _) = hier.predict(Some("playlist"));
        // Press should be well above playlist.
        assert!(
            press_prob > playlist_prob,
            "press (20 successes) should be above playlist (20 failures), got press={press_prob} playlist={playlist_prob}"
        );
        assert!(
            press_prob > 0.3,
            "press with 20 successes should be well above 0.1, got {press_prob}"
        );
    }

    #[test]
    fn hierarchical_beta_target_shrinks_toward_kind() {
        let mut hier = HierarchicalBetaPosterior::new(BetaPosterior::from_mean(0.1, 10.0));
        // Give "playlist" kind data at ~30% reply rate.
        for _ in 0..10 {
            hier.update(Some("playlist"), None, true);
        }
        for _ in 0..20 {
            hier.update(Some("playlist"), None, false);
        }
        let (kind_prob, _) = hier.predict(Some("playlist"));
        // One observation for a specific target — should shrink toward kind.
        hier.update(Some("playlist"), Some("curator:good"), true);
        let (target_prob, _) = hier.predict_for_target(Some("playlist"), Some("curator:good"));
        assert!(
            target_prob > kind_prob,
            "one positive observation should pull target above kind, got target={target_prob} kind={kind_prob}"
        );
        // But not by much — one observation should not overwhelm the kind.
        assert!(
            target_prob < kind_prob + 0.2,
            "one observation must not overwhelm the kind: target={target_prob} kind={kind_prob}"
        );
    }

    #[test]
    fn hierarchical_beta_no_double_counting() {
        // With one observation of success for kind "a":
        // CORRECT: global prior = Beta(1, 1), global post = Beta(2, 1)
        //          kind "a" prior = Beta(1, 1), kind "a" post = Beta(2, 1)
        // WRONG (double-count): kind "a" prior = global_post = Beta(2, 1),
        //          then update: kind "a" = Beta(3, 1)
        let mut hier = HierarchicalBetaPosterior::new(BetaPosterior::prior(1.0, 1.0));
        hier.update(Some("a"), None, true);
        let kind_post = hier.by_kind.get("a").unwrap();
        assert!(
            (kind_post.alpha - 2.0).abs() < 1e-12,
            "kind alpha should be 2 (pre-update global + 1 success), got {}",
            kind_post.alpha
        );
        assert!(
            (kind_post.beta - 1.0).abs() < 1e-12,
            "kind beta should be 1 (pre-update global + 0 failures), got {}",
            kind_post.beta
        );
        // If double-counted: alpha would be 3.
    }

    #[test]
    fn hierarchical_beta_target_confidence_counts_only_that_target() {
        let mut hier = HierarchicalBetaPosterior::new(BetaPosterior::prior(1.0, 1.0));
        hier.update(Some("playlist"), Some("curator:a"), true);
        hier.update(Some("playlist"), Some("curator:b"), false);
        hier.update(Some("playlist"), Some("curator:a"), true);
        assert_eq!(hier.target_confidence("curator:a"), 2);
        assert_eq!(hier.target_confidence("curator:b"), 1);
        assert_eq!(hier.target_confidence("curator:missing"), 0);
        assert_eq!(hier.kind_confidence("playlist"), 3);
    }

    #[test]
    fn hierarchical_beta_checkpoint_roundtrip() {
        let mut hier = HierarchicalBetaPosterior::new(BetaPosterior::from_mean(0.1, 10.0));
        for _ in 0..5 {
            hier.update(Some("press"), Some("target:x"), true);
        }
        let json = serde_json::to_string(&hier).expect("serialize");
        let restored: HierarchicalBetaPosterior = serde_json::from_str(&json).expect("deserialize");
        let (orig_prob, _) = hier.predict_for_target(Some("press"), Some("target:x"));
        let (restored_prob, _) = restored.predict_for_target(Some("press"), Some("target:x"));
        assert!(
            (orig_prob - restored_prob).abs() < 1e-12,
            "roundtrip should preserve predictions: {orig_prob} vs {restored_prob}"
        );
    }

    #[test]
    fn hierarchical_beta_legacy_checkpoint_without_target_level_loads() {
        // A checkpoint written before by_target existed must still load.
        let json = r#"{"global":{"alpha":1.0,"beta":9.0,"n":0},"by_kind":{}}"#;
        let h: HierarchicalBetaPosterior =
            serde_json::from_str(json).expect("legacy checkpoint should deserialize");
        assert!(h.by_target.is_empty());
        assert!((h.global.mean() - 0.1).abs() < 1e-12);
    }
}
