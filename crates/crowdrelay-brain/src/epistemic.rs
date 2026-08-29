//! Epistemic frontier — the brain's map of what it doesn't know.
//!
//! The epistemic frontier is the set of (template, context) pairs where the
//! brain has the highest uncertainty about the treatment effect. These are
//! the pairs where dispatching would produce the most information gain.
//!
//! The brain uses the epistemic frontier to:
//! 1. Identify which templates it should experiment with next.
//! 2. Avoid over-dispatching templates it already knows well.
//! 3. Balance exploitation (high expected value) with exploration
//!    (high information gain).
//!
//! # Frontier computation
//!
//! For each (template, context) pair, the brain computes:
//! - `treatment_std`: the uncertainty in the treatment effect.
//! - `treatment_confidence`: the number of paired observations.
//! - `epistemic_value`: `treatment_std × (1 - confidence / threshold)`.
//!
//! The epistemic value is high when the uncertainty is high AND the
//! confidence is low. As confidence increases, the epistemic value
//! decreases — the brain has learned what it needed to learn.

use serde::{Deserialize, Serialize};

/// An entry on the epistemic frontier — a (template, context) pair where
/// the brain has high uncertainty.
#[allow(dead_code)] // TODO: wire into production path (next sprint)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EpistemicFrontierEntry {
    /// The template ID.
    pub template_id: String,
    /// The subreddit type context (if any).
    pub subreddit_type: Option<String>,
    /// The treatment-effect standard deviation (uncertainty).
    pub treatment_std: f64,
    /// The treatment-effect confidence (paired observation count).
    pub treatment_confidence: u32,
    /// The epistemic value — how much the brain would learn from dispatching.
    pub epistemic_value: f64,
}

/// The epistemic frontier — a ranked list of (template, context) pairs
/// by epistemic value.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EpistemicFrontier {
    /// Entries sorted by epistemic value (descending).
    pub entries: Vec<EpistemicFrontierEntry>,
}

impl EpistemicFrontier {
    /// Creates a new empty frontier.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds the frontier from a list of (template, subreddit_type, std, confidence) tuples.
    ///
    /// The epistemic value is computed as:
    /// ```text
    /// epistemic_value = std × max(0, 1 - confidence / threshold)
    /// ```
    ///
    /// Pairs with high uncertainty and low confidence get the highest
    /// epistemic value. As confidence approaches the threshold, the value
    /// drops to zero — the brain has learned enough.
    #[must_use]
    pub fn from_uncertainties(
        entries: impl IntoIterator<Item = (String, Option<String>, f64, u32)>,
        confidence_threshold: u32,
    ) -> Self {
        let mut frontier: Vec<EpistemicFrontierEntry> = entries
            .into_iter()
            .map(|(template_id, subreddit_type, std, conf)| {
                let epistemic_value =
                    std * (1.0 - (conf as f64 / confidence_threshold as f64).min(1.0)).max(0.0);
                EpistemicFrontierEntry {
                    template_id,
                    subreddit_type,
                    treatment_std: std,
                    treatment_confidence: conf,
                    epistemic_value,
                }
            })
            .filter(|e| e.epistemic_value > 0.0)
            .collect();
        // Sort by epistemic value descending.
        frontier.sort_by(|a, b| {
            b.epistemic_value
                .partial_cmp(&a.epistemic_value)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Self { entries: frontier }
    }

    /// Returns the top N frontier entries.
    #[must_use]
    pub fn top(&self, n: usize) -> &[EpistemicFrontierEntry] {
        let n = n.min(self.entries.len());
        &self.entries[..n]
    }

    /// Returns true if the frontier is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the number of entries on the frontier.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontier_ranks_by_epistemic_value() {
        let frontier = EpistemicFrontier::from_uncertainties(
            [
                ("a".to_owned(), None, 1.0, 0),  // high value: high std, no confidence
                ("b".to_owned(), None, 1.0, 10), // low value: high std, but confident
                ("c".to_owned(), None, 0.1, 0),  // low value: low std
            ],
            10,
        );
        assert_eq!(frontier.len(), 2); // "b" is filtered out (epistemic_value = 0)
        assert_eq!(frontier.entries[0].template_id, "a");
        assert!(frontier.entries[0].epistemic_value > frontier.entries[1].epistemic_value);
    }

    #[test]
    fn frontier_top_n() {
        let frontier = EpistemicFrontier::from_uncertainties(
            [
                ("a".to_owned(), None, 2.0, 0),
                ("b".to_owned(), None, 1.5, 0),
                ("c".to_owned(), None, 1.0, 0),
            ],
            10,
        );
        let top2 = frontier.top(2);
        assert_eq!(top2.len(), 2);
        assert_eq!(top2[0].template_id, "a");
        assert_eq!(top2[1].template_id, "b");
    }

    #[test]
    fn frontier_filters_zero_value() {
        let frontier = EpistemicFrontier::from_uncertainties(
            [("a".to_owned(), None, 1.0, 10)],
            10, // confidence = threshold → epistemic_value = 0
        );
        assert!(frontier.is_empty());
    }

    #[test]
    fn frontier_empty_when_no_entries() {
        let frontier = EpistemicFrontier::from_uncertainties(std::iter::empty(), 10);
        assert!(frontier.is_empty());
    }
}
