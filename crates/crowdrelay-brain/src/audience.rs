//! Audience overlap — marginal value computation for multi-channel dispatch.
//!
//! When the brain dispatches multiple workers targeting overlapping audiences,
//! the marginal value of each additional dispatch decreases. A reddit post
//! and a social media post targeting the same metal fans don't produce
//! independent fan acquisitions — many fans who see both will only join once.
//!
//! # Overlap model
//!
//! The brain models audience overlap using a simple set-intersection model:
//!
//! ```text
//! marginal_value(opp_i | already_selected) =
//!     expected_fans_i × (1 - overlap_penalty × |already_selected ∩ audience_i|)
//! ```
//!
//! The overlap penalty is configurable. At 0.0, there's no penalty (audiences
//! are assumed independent). At 1.0, any overlap fully zeroes the marginal
//! value (same audience is fully saturated).

use serde::Serialize;
use std::collections::HashMap;

/// An audience identifier — a set of fans or potential fans that a dispatch
/// targets. Two dispatches with the same audience key target the same audience.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize)]
pub struct AudienceKey(String);

impl AudienceKey {
    /// Creates an audience key for a subreddit.
    #[must_use]
    pub fn subreddit(name: &str) -> Self {
        Self(format!("subreddit:{name}"))
    }

    /// Creates an audience key for a venue/city.
    #[must_use]
    pub fn venue(name: &str) -> Self {
        Self(format!("venue:{name}"))
    }

    /// Creates an audience key for a platform (Spotify, Meta, etc.).
    #[must_use]
    pub fn platform(name: &str) -> Self {
        Self(format!("platform:{name}"))
    }

    /// Creates an audience key for the global audience (all fans).
    #[must_use]
    pub fn global() -> Self {
        Self("global".to_owned())
    }

    /// Creates a custom audience key.
    #[must_use]
    pub fn custom(key: &str) -> Self {
        Self(key.to_owned())
    }

    /// Returns the string representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AudienceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Computes the marginal value of a dispatch given the already-selected
/// dispatches and their audience overlap.
///
/// ```text
/// marginal = expected_fans × (1 - penalty × overlap_count).max(0)
/// ```
#[must_use]
pub fn marginal_value(
    expected_fans: f64,
    audience_key: &AudienceKey,
    selected_audiences: &HashMap<String, u32>,
    overlap_penalty: f64,
) -> f64 {
    let overlap_count = selected_audiences
        .get(audience_key.as_str())
        .copied()
        .unwrap_or(0);
    let factor = (1.0 - overlap_penalty * overlap_count as f64).max(0.0);
    expected_fans * factor
}

/// Estimates the audience overlap between two audience keys based on their
/// type and specificity. Returns a value in [0, 1]:
/// - 0.0 = no overlap (completely different audiences)
/// - 1.0 = full overlap (same audience)
#[must_use]
pub fn estimate_overlap(key_a: &AudienceKey, key_b: &AudienceKey) -> f64 {
    if key_a == key_b {
        return 1.0;
    }
    let a = key_a.as_str();
    let b = key_b.as_str();
    // Same type, different target: partial overlap.
    if let Some((type_a, _)) = a.split_once(':')
        && let Some((type_b, _)) = b.split_once(':')
        && type_a == type_b
    {
        // Same type (e.g. both subreddits): some audience overlap.
        return 0.2;
    }
    // Global overlaps with everything.
    if a == "global" || b == "global" {
        return 0.5;
    }
    // Different types: minimal overlap.
    0.05
}

/// A learned audience overlap model — stores observed overlap coefficients
/// between audience pairs and falls back to [`estimate_overlap`] when no
/// learned data exists.
///
/// The brain learns overlap from observed outcomes: when two dispatches to
/// overlapping audiences produce fewer combined fans than the sum of their
/// individual predictions, the difference is attributed to overlap. This
/// is more accurate than the hardcoded heuristic in [`estimate_overlap`]
/// because it accounts for the specific audience relationships in the
/// tenant's fanbase.
///
/// # Learning rule
///
/// When the brain dispatches to audiences A and B and observes combined
/// outcome `Y_AB`, compared to the expected `Y_A + Y_B`, the overlap
/// coefficient is updated:
///
/// ```text
/// overlap(A, B) = (Y_A + Y_B - Y_AB) / Y_A   (if Y_A > 0)
/// ```
///
/// This is stored as a running average keyed by the sorted pair `(A, B)`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct LearnedOverlapModel {
    /// Learned overlap coefficients, keyed by "audience_a|audience_b" (sorted).
    /// Values are in [0, 1].
    pub coefficients: HashMap<String, f64>,
    /// Number of observations per pair (for running average).
    pub counts: HashMap<String, u32>,
}

impl LearnedOverlapModel {
    /// Creates a new, empty learned overlap model.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the overlap between two audiences. Uses the learned coefficient
    /// if available, otherwise falls back to [`estimate_overlap`].
    #[must_use]
    pub fn overlap(&self, key_a: &AudienceKey, key_b: &AudienceKey) -> f64 {
        if key_a == key_b {
            return 1.0;
        }
        let pair_key = Self::pair_key(key_a, key_b);
        self.coefficients
            .get(&pair_key)
            .copied()
            .unwrap_or_else(|| estimate_overlap(key_a, key_b))
    }

    /// Updates the learned overlap coefficient for an audience pair from an
    /// observed outcome.
    ///
    /// `expected_sum` is the sum of individual predictions (Y_A + Y_B).
    /// `observed_combined` is the actual combined outcome (Y_AB).
    /// `reference` is the larger of the two individual predictions (used as
    /// the denominator for the overlap fraction).
    pub fn update(
        &mut self,
        key_a: &AudienceKey,
        key_b: &AudienceKey,
        expected_sum: f64,
        observed_combined: f64,
        reference: f64,
    ) {
        if key_a == key_b || reference <= 0.0 {
            return;
        }
        let observed_overlap = ((expected_sum - observed_combined) / reference).clamp(0.0, 1.0);
        let pair_key = Self::pair_key(key_a, key_b);
        let count = self.counts.get(&pair_key).copied().unwrap_or(0);
        let current = self.coefficients.get(&pair_key).copied().unwrap_or(0.0);
        // Running average.
        let updated = if count == 0 {
            observed_overlap
        } else {
            (current * count as f64 + observed_overlap) / (count as f64 + 1.0)
        };
        self.coefficients.insert(pair_key.clone(), updated);
        self.counts.insert(pair_key, count + 1);
    }

    /// Creates a sorted pair key for two audience keys.
    fn pair_key(a: &AudienceKey, b: &AudienceKey) -> String {
        if a.as_str() <= b.as_str() {
            format!("{}|{}", a, b)
        } else {
            format!("{}|{}", b, a)
        }
    }

    /// Returns the number of learned overlap pairs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.coefficients.len()
    }

    /// Returns true if no overlap coefficients have been learned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.coefficients.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subreddit_audience_key_format() {
        assert_eq!(
            AudienceKey::subreddit("MetalMusic").as_str(),
            "subreddit:MetalMusic"
        );
    }

    #[test]
    fn venue_audience_key_format() {
        assert_eq!(
            AudienceKey::venue("Warsaw_Palladium").as_str(),
            "venue:Warsaw_Palladium"
        );
    }

    #[test]
    fn global_audience_key() {
        assert_eq!(AudienceKey::global().as_str(), "global");
    }

    #[test]
    fn marginal_value_no_overlap() {
        let selected = HashMap::new();
        let audience = AudienceKey::subreddit("MetalMusic");
        let marginal = marginal_value(10.0, &audience, &selected, 0.3);
        assert!((marginal - 10.0).abs() < 0.01);
    }

    #[test]
    fn marginal_value_with_overlap() {
        let mut selected = HashMap::new();
        selected.insert("subreddit:MetalMusic".to_owned(), 1);
        let audience = AudienceKey::subreddit("MetalMusic");
        let marginal = marginal_value(10.0, &audience, &selected, 0.3);
        // 10.0 * (1 - 0.3 * 1) = 7.0
        assert!((marginal - 7.0).abs() < 0.01);
    }

    #[test]
    fn marginal_value_full_overlap_zeros_out() {
        let mut selected = HashMap::new();
        selected.insert("subreddit:MetalMusic".to_owned(), 3);
        let audience = AudienceKey::subreddit("MetalMusic");
        let marginal = marginal_value(10.0, &audience, &selected, 1.0);
        // 10.0 * (1 - 1.0 * 3).max(0) = 0.0
        assert!((marginal - 0.0).abs() < 0.01);
    }

    #[test]
    fn marginal_value_clamps_to_zero() {
        let mut selected = HashMap::new();
        selected.insert("subreddit:MetalMusic".to_owned(), 10);
        let audience = AudienceKey::subreddit("MetalMusic");
        let marginal = marginal_value(10.0, &audience, &selected, 0.5);
        // 10.0 * (1 - 0.5 * 10).max(0) = 10.0 * 0.0 = 0.0
        assert!((marginal - 0.0).abs() < 0.01);
    }

    #[test]
    fn estimate_overlap_same_key_is_full() {
        let a = AudienceKey::subreddit("MetalMusic");
        let b = AudienceKey::subreddit("MetalMusic");
        assert!((estimate_overlap(&a, &b) - 1.0).abs() < 0.01);
    }

    #[test]
    fn estimate_overlap_same_type_different_target() {
        let a = AudienceKey::subreddit("MetalMusic");
        let b = AudienceKey::subreddit("ProgMusic");
        assert!((estimate_overlap(&a, &b) - 0.2).abs() < 0.01);
    }

    #[test]
    fn estimate_overlap_global_with_anything() {
        let global = AudienceKey::global();
        let sub = AudienceKey::subreddit("MetalMusic");
        assert!((estimate_overlap(&global, &sub) - 0.5).abs() < 0.01);
    }

    #[test]
    fn estimate_overlap_different_types() {
        let sub = AudienceKey::subreddit("MetalMusic");
        let venue = AudienceKey::venue("Warsaw");
        assert!((estimate_overlap(&sub, &venue) - 0.05).abs() < 0.01);
    }

    #[test]
    fn audience_key_display() {
        let key = AudienceKey::subreddit("MetalMusic");
        assert_eq!(key.to_string(), "subreddit:MetalMusic");
    }

    #[test]
    fn audience_key_equality() {
        assert_eq!(
            AudienceKey::subreddit("Metal"),
            AudienceKey::subreddit("Metal")
        );
        assert_ne!(
            AudienceKey::subreddit("Metal"),
            AudienceKey::subreddit("Prog")
        );
    }

    #[test]
    fn custom_audience_key() {
        assert_eq!(
            AudienceKey::custom("custom_audience").as_str(),
            "custom_audience"
        );
    }

    #[test]
    fn platform_audience_key() {
        assert_eq!(
            AudienceKey::platform("Spotify").as_str(),
            "platform:Spotify"
        );
    }

    // ─── LearnedOverlapModel tests ────────────────────────────────────────

    #[test]
    fn learned_overlap_starts_empty() {
        let model = LearnedOverlapModel::new();
        assert!(model.is_empty());
        assert_eq!(model.len(), 0);
    }

    #[test]
    fn learned_overlap_falls_back_to_heuristic() {
        let model = LearnedOverlapModel::new();
        let a = AudienceKey::subreddit("MetalMusic");
        let b = AudienceKey::subreddit("ProgMusic");
        // No learned data → should use heuristic (0.2 for same type).
        assert!((model.overlap(&a, &b) - 0.2).abs() < 0.01);
    }

    #[test]
    fn learned_overlap_same_key_is_full() {
        let model = LearnedOverlapModel::new();
        let a = AudienceKey::subreddit("MetalMusic");
        assert!((model.overlap(&a, &a) - 1.0).abs() < 0.01);
    }

    #[test]
    fn learned_overlap_updates_from_observation() {
        let mut model = LearnedOverlapModel::new();
        let a = AudienceKey::subreddit("MetalMusic");
        let b = AudienceKey::subreddit("ProgMusic");
        // Expected sum = 10, observed = 7, reference = 10.
        // overlap = (10 - 7) / 10 = 0.3
        model.update(&a, &b, 10.0, 7.0, 10.0);
        assert!((model.overlap(&a, &b) - 0.3).abs() < 0.01);
        assert_eq!(model.len(), 1);
    }

    #[test]
    fn learned_overlap_averages_multiple_observations() {
        let mut model = LearnedOverlapModel::new();
        let a = AudienceKey::subreddit("MetalMusic");
        let b = AudienceKey::subreddit("ProgMusic");
        // First: overlap = 0.3
        model.update(&a, &b, 10.0, 7.0, 10.0);
        // Second: overlap = 0.5
        model.update(&a, &b, 10.0, 5.0, 10.0);
        // Average = (0.3 + 0.5) / 2 = 0.4
        assert!((model.overlap(&a, &b) - 0.4).abs() < 0.01);
    }

    #[test]
    fn learned_overlap_clamps_to_zero_one() {
        let mut model = LearnedOverlapModel::new();
        let a = AudienceKey::subreddit("MetalMusic");
        let b = AudienceKey::subreddit("ProgMusic");
        // Observed > expected → overlap = negative → clamped to 0.
        model.update(&a, &b, 10.0, 15.0, 10.0);
        assert!((model.overlap(&a, &b) - 0.0).abs() < 0.01);
    }

    #[test]
    fn learned_overlap_ignores_same_key() {
        let mut model = LearnedOverlapModel::new();
        let a = AudienceKey::subreddit("MetalMusic");
        model.update(&a, &a, 10.0, 5.0, 10.0);
        assert!(model.is_empty());
    }

    #[test]
    fn learned_overlap_ignores_zero_reference() {
        let mut model = LearnedOverlapModel::new();
        let a = AudienceKey::subreddit("MetalMusic");
        let b = AudienceKey::subreddit("ProgMusic");
        model.update(&a, &b, 0.0, 0.0, 0.0);
        assert!(model.is_empty());
    }

    #[test]
    fn learned_overlap_pair_key_is_symmetric() {
        let mut model = LearnedOverlapModel::new();
        let a = AudienceKey::subreddit("MetalMusic");
        let b = AudienceKey::subreddit("ProgMusic");
        // Update with (a, b) then query with (b, a) — should find the same
        // coefficient.
        model.update(&a, &b, 10.0, 7.0, 10.0);
        assert!((model.overlap(&b, &a) - 0.3).abs() < 0.01);
    }
}
