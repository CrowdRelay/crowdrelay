//! Compact continuity for completed growth evaluations, never preview writes.

use super::{BrainState, MetacognitionMonitor};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MetacognitionObservation {
    pub metric: String,
    pub state: BrainState,
    pub observed_at_micros: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MetacognitionCheckpoint {
    pub metric: String,
    pub observed_at_micros: i64,
    pub monitor: MetacognitionMonitor,
}

impl MetacognitionCheckpoint {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            metric: String::new(),
            observed_at_micros: i64::MIN,
            monitor: MetacognitionMonitor::new(),
        }
    }

    /// A repeated or older evaluation cannot advance or overwrite continuity.
    /// Changing the optimized metric starts a new, comparable assessment run.
    #[must_use]
    pub fn advance(&self, observation: &MetacognitionObservation) -> Self {
        if observation.observed_at_micros <= self.observed_at_micros {
            return self.clone();
        }
        let mut monitor = if observation.metric == self.metric {
            self.monitor.clone()
        } else {
            MetacognitionMonitor::new()
        };
        monitor.observe(observation.state);
        Self {
            metric: observation.metric.clone(),
            observed_at_micros: observation.observed_at_micros,
            monitor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(metric: &str, state: BrainState, at: i64) -> MetacognitionObservation {
        MetacognitionObservation {
            metric: metric.into(),
            state,
            observed_at_micros: at,
        }
    }

    #[test]
    fn completed_evaluations_accumulate_without_growing_history() {
        let mut checkpoint = MetacognitionCheckpoint::empty();
        for at in 1..=100_000 {
            checkpoint = checkpoint.advance(&observation("spotify", BrainState::Learning, at));
        }
        assert_eq!(checkpoint.monitor.learning_cycles, 100_000);
        assert_eq!(checkpoint.monitor.stagnation_escape_boost(), 0.5);
        assert_eq!(
            checkpoint.monitor.sizing_multiplier(),
            BrainState::Learning.sizing_multiplier()
        );
    }

    #[test]
    fn retry_and_stale_evaluation_do_not_count_twice() {
        let checkpoint = MetacognitionCheckpoint::empty().advance(&observation(
            "spotify",
            BrainState::Learning,
            10,
        ));
        for at in [10, 9] {
            let repeated = checkpoint.advance(&observation("signal", BrainState::Regressing, at));
            assert_eq!(repeated.metric, "spotify");
            assert_eq!(repeated.monitor.learning_cycles, 1);
            assert_eq!(repeated.monitor.state, BrainState::Learning);
        }
    }

    #[test]
    fn a_preview_projection_leaves_the_checkpoint_unchanged() {
        let checkpoint = MetacognitionCheckpoint::empty().advance(&observation(
            "spotify",
            BrainState::Learning,
            10,
        ));
        for _ in 0..100 {
            assert_eq!(
                checkpoint
                    .advance(&observation("spotify", BrainState::Learning, 11))
                    .monitor
                    .learning_cycles,
                2
            );
        }
        assert_eq!(checkpoint.monitor.learning_cycles, 1);
    }

    #[test]
    fn state_and_metric_changes_reset_the_comparable_streak() {
        let checkpoint = MetacognitionCheckpoint::empty()
            .advance(&observation("spotify", BrainState::Learning, 10))
            .advance(&observation("spotify", BrainState::Improving, 11));
        assert_eq!(checkpoint.monitor.learning_cycles, 0);
        assert_eq!(checkpoint.monitor.improving_cycles_total, 1);
        let changed =
            checkpoint.advance(&observation("activated_fans_30d", BrainState::Learning, 12));
        assert_eq!(changed.monitor.learning_cycles, 1);
        assert_eq!(changed.monitor.improving_cycles_total, 0);
    }

    #[test]
    fn counters_saturate_instead_of_panicking_or_wrapping() {
        let mut monitor = MetacognitionMonitor::new();
        monitor.state = BrainState::Learning;
        monitor.learning_cycles = u32::MAX;
        monitor.observe(BrainState::Learning);
        assert_eq!(monitor.learning_cycles, u32::MAX);
        assert_eq!(monitor.stagnation_escape_boost(), 0.5);
        monitor.improving_cycles_total = u32::MAX;
        monitor.observe(BrainState::Improving);
        assert_eq!(monitor.improving_cycles_total, u32::MAX);
    }
}
