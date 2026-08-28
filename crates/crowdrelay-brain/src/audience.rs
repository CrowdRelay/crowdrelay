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
}
