//! Go-Explore memory — exploration archive for active information seeking.
//!
//! The brain remembers which (template, context) combinations it has already
//! explored, so it doesn't waste dispatches re-exploring known territory.
//! Novel combinations get an exploration bonus; well-explored ones get
//! diminishing returns.

use serde::Serialize;
use std::collections::HashMap;

use crate::causal_model::DispatchContext;
use crate::world_model::GrowthTrend;

/// The decay rate for recency-weighted visits.
pub const VISIT_DECAY: f64 = 0.95;

/// The cross-template generalization factor.
pub const CROSS_TEMPLATE_FACTOR: f64 = 0.3;

/// The brain's exploration memory.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ExplorationMemory {
    /// Map of (template_id, context_hash) → effective visit count.
    pub visits: HashMap<String, f64>,
    /// Map of context_hash → effective visit count (across all templates).
    pub context_visits: HashMap<String, f64>,
}

impl ExplorationMemory {
    /// Records a visit to a (template, context) pair.
    pub fn record_visit(&mut self, template_id: &str, context_hash: &str) {
        let key = format!("{template_id}:{context_hash}");
        *self.visits.entry(key).or_insert(0.0) += 1.0;
        *self
            .context_visits
            .entry(context_hash.to_owned())
            .or_insert(0.0) += 1.0;
    }

    /// Records a visit with a pre-computed decayed weight. Used when
    /// reconstructing the memory from the database: each historical visit
    /// is weighted by `VISIT_DECAY^age_cycles` so old visits contribute less.
    ///
    /// This fixes the bug where `load_exploration_memory()` loaded ALL
    /// historical predictions with full weight, making 6-month-old visits
    /// count the same as yesterday's.
    pub fn record_decayed_visit(&mut self, template_id: &str, context_hash: &str, weight: f64) {
        if weight <= 0.0 {
            return;
        }
        let key = format!("{template_id}:{context_hash}");
        *self.visits.entry(key).or_insert(0.0) += weight;
        *self
            .context_visits
            .entry(context_hash.to_owned())
            .or_insert(0.0) += weight;
    }

    /// Returns the novelty score for a (template, context) pair.
    #[must_use]
    pub fn novelty(&self, template_id: &str, context_hash: &str) -> f64 {
        let key = format!("{template_id}:{context_hash}");
        let template_visits = self.visits.get(&key).copied().unwrap_or(0.0);
        let context_visits = self
            .context_visits
            .get(context_hash)
            .copied()
            .unwrap_or(0.0);
        let effective = template_visits + CROSS_TEMPLATE_FACTOR * context_visits;
        1.0 / (1.0 + effective)
    }

    /// Returns the total number of unique (template, context) pairs explored.
    #[must_use]
    pub fn explored_count(&self) -> usize {
        self.visits.len()
    }

    /// Decays all visit counts by the decay factor.
    pub fn decay(&mut self) {
        for v in self.visits.values_mut() {
            *v *= VISIT_DECAY;
            if *v < 0.01 {
                *v = 0.0;
            }
        }
        for v in self.context_visits.values_mut() {
            *v *= VISIT_DECAY;
            if *v < 0.01 {
                *v = 0.0;
            }
        }
        self.visits.retain(|_, v| *v > 0.0);
        self.context_visits.retain(|_, v| *v > 0.0);
    }
}

/// Computes a context hash from a DispatchContext for exploration tracking.
#[must_use]
pub fn context_hash(context: &DispatchContext) -> String {
    use std::fmt::Write;
    let event_bucket = match context.days_to_event {
        None => 0u8,
        Some(0..=1) => 1,
        Some(2..=7) => 2,
        Some(8..=14) => 3,
        Some(15..=30) => 4,
        Some(_) => 5,
    };
    let trend = match context.fan_growth_trend {
        GrowthTrend::Accelerating => "Acc",
        GrowthTrend::Steady => "Std",
        GrowthTrend::Decelerating => "Dec",
        GrowthTrend::Stagnant => "Stg",
    };
    let sub = context.subreddit_type.as_deref().unwrap_or("");
    let fmt = context.post_format.as_deref().unwrap_or("");
    let cap = 4 + trend.len() + sub.len() + fmt.len() + 3 + 6 + 6;
    let mut s = String::with_capacity(cap);
    // Write directly into the pre-allocated String to avoid temporary
    // String allocations from .to_string() on each numeric field.
    write!(s, "{event_bucket}:{trend}:{sub}:{fmt}").unwrap();
    write!(
        s,
        ":{}:{}",
        context.time_of_day_bps, context.community_novelty_bps
    )
    .unwrap();
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exploration_memory_novel_for_unvisited() {
        let mem = ExplorationMemory::default();
        assert!((mem.novelty("reddit-scanner", "ctx1") - 1.0).abs() < 0.01);
    }

    #[test]
    fn exploration_memory_novelty_decreases_with_visits() {
        let mut mem = ExplorationMemory::default();
        mem.record_visit("reddit-scanner", "ctx1");
        let novelty_after_1 = mem.novelty("reddit-scanner", "ctx1");
        assert!(novelty_after_1 < 0.5 && novelty_after_1 > 0.0);
        mem.record_visit("reddit-scanner", "ctx1");
        let novelty_after_2 = mem.novelty("reddit-scanner", "ctx1");
        assert!(novelty_after_2 < novelty_after_1);
    }

    #[test]
    fn exploration_memory_tracks_unique_pairs() {
        let mut mem = ExplorationMemory::default();
        mem.record_visit("a", "x");
        mem.record_visit("a", "x");
        mem.record_visit("b", "y");
        assert_eq!(mem.explored_count(), 2);
    }

    #[test]
    fn exploration_memory_cross_template_generalization() {
        let mut mem = ExplorationMemory::default();
        mem.record_visit("a", "ctx1");
        let novelty_b_same_ctx = mem.novelty("b", "ctx1");
        let novelty_b_new_ctx = mem.novelty("b", "ctx2");
        assert!(novelty_b_same_ctx < novelty_b_new_ctx);
        let novelty_a_same_ctx = mem.novelty("a", "ctx1");
        assert!(novelty_b_same_ctx > novelty_a_same_ctx);
    }

    #[test]
    fn exploration_memory_decay_makes_old_visits_novel_again() {
        let mut mem = ExplorationMemory::default();
        mem.record_visit("a", "ctx1");
        let novelty_before = mem.novelty("a", "ctx1");
        for _ in 0..100 {
            mem.decay();
        }
        let novelty_after = mem.novelty("a", "ctx1");
        assert!(novelty_after > novelty_before);
        assert!(novelty_after > 0.9);
    }

    #[test]
    fn exploration_memory_decay_cleans_up_zero_entries() {
        let mut mem = ExplorationMemory::default();
        mem.record_visit("a", "ctx1");
        mem.record_visit("b", "ctx2");
        for _ in 0..200 {
            mem.decay();
        }
        assert!(mem.visits.is_empty());
        assert!(mem.context_visits.is_empty());
    }

    #[test]
    fn record_decayed_visit_weights_old_visits_less() {
        let mut mem = ExplorationMemory::default();
        // A recent visit (full weight).
        mem.record_decayed_visit("a", "ctx1", 1.0);
        // An old visit (decayed to 0.1).
        mem.record_decayed_visit("a", "ctx1", 0.1);
        // Total weight = 1.1, which is less than 2.0 (two full visits).
        let novelty = mem.novelty("a", "ctx1");
        let novelty_two_full = {
            let mut m2 = ExplorationMemory::default();
            m2.record_visit("a", "ctx1");
            m2.record_visit("a", "ctx1");
            m2.novelty("a", "ctx1")
        };
        assert!(
            novelty > novelty_two_full,
            "decayed visits should result in higher novelty (less explored)"
        );
    }

    #[test]
    fn record_decayed_visit_ignores_zero_weight() {
        let mut mem = ExplorationMemory::default();
        mem.record_decayed_visit("a", "ctx1", 0.0);
        assert_eq!(mem.explored_count(), 0);
    }

    #[test]
    fn context_hash_distinguishes_different_contexts() {
        let ctx1 = DispatchContext {
            days_to_event: Some(5),
            ..Default::default()
        };
        let ctx2 = DispatchContext {
            days_to_event: Some(30),
            ..Default::default()
        };
        assert_ne!(context_hash(&ctx1), context_hash(&ctx2));
    }

    #[test]
    fn context_hash_buckets_nearby_event_days() {
        let ctx3 = DispatchContext {
            days_to_event: Some(3),
            ..Default::default()
        };
        let ctx5 = DispatchContext {
            days_to_event: Some(5),
            ..Default::default()
        };
        assert_eq!(context_hash(&ctx3), context_hash(&ctx5));
        let ctx10 = DispatchContext {
            days_to_event: Some(10),
            ..Default::default()
        };
        assert_ne!(context_hash(&ctx3), context_hash(&ctx10));
    }

    #[test]
    fn context_hash_same_for_same_context() {
        let ctx = DispatchContext {
            days_to_event: Some(5),
            fan_growth_trend: GrowthTrend::Stagnant,
            subreddit_type: Some("metal".to_owned()),
            post_format: Some("text".to_owned()),
            ..Default::default()
        };
        assert_eq!(context_hash(&ctx), context_hash(&ctx));
    }
}
