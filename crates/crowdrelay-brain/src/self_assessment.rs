//! What the brain thinks of its own performance, in its own words — and
//! how that assessment feeds back into behavior.
//!
//! Ported from Kern's `brain::metacognition`, which is the same author's
//! engine applied to capital. The transferable part is not the arithmetic —
//! a split-half comparison is not an idea — it is the vocabulary and one
//! judgement embedded in it: **a brain with no result yet is learning, not
//! stuck**. Kern says so directly ("when the brain has no alpha yet, it is
//! learning, not stuck"), and CrowdRelay had no way to say it at all.
//!
//! That silence is what this fixes. The brain ran sixteen cycles producing
//! nothing and reported success on every one, because "every phase completed"
//! and "the brain achieved anything" were the same field. An operator watching
//! a fan count sit at ten had to infer the difference, and the obvious
//! inference — that the thing is broken — was wrong.
//!
//! # Behavior feedback (now wired in)
//!
//! Kern's states carry `exploration_boost` and `sizing_multiplier`, which feed
//! back into ranking and position size. CrowdRelay's `DecisionValue` has an
//! explicit invariant against terms entering `total()` without a defined
//! conversion into fan-equivalent utility. This module respects that
//! invariant:
//!
//! - `exploration_boost()` enters **EFE weights** (exploration), not
//!   `DecisionValue.total()`. EFE decides what is worth learning about;
//!   DecisionValue decides what is worth doing.
//! - `sizing_multiplier()` enters **dispatch budget** (a constraint), not
//!   a value term. It scales how many dispatches a template gets, not
//!   how much each dispatch is worth.
//!
//! This is the same separation Kern uses. The brain's self-assessment
//! changes how hard it explores and how much budget it allocates, but
//! does not inflate the value of any individual action.
//!
//! # Timescale
//!
//! Kern observes per cycle at 30-second cadence, so ten samples is five
//! minutes and alpha genuinely moves inside the window. CrowdRelay cycles
//! every five minutes and its North Star is fans, which move over days. The
//! same window in cycles would compare 09:00 with 09:50 and find every honest
//! answer to be "flat", reporting `Learning` forever and meaning nothing.
//!
//! So the window here is in days, not cycles, and a run of samples taken
//! inside one day is one observation. The state is only claimed once there
//! are enough distinct days to compare.

use serde::{Deserialize, Serialize};

/// The fewest distinct days needed before a trend is claimed at all.
///
/// Six lets three be compared with three. Below that the brain says
/// `Initializing`, which is the honest answer for a system that has not
/// watched itself for long enough to have an opinion.
const MINIMUM_DAYS: usize = 6;

/// How much the North Star has to move for the change to count as real,
/// as a fraction of the earlier level.
///
/// Proportional rather than absolute because one extra fan out of ten is a
/// different event from one out of a thousand, and a fixed threshold would
/// call the first noise and the second a triumph.
const MATERIAL_CHANGE: f64 = 0.05;

/// The brain's assessment of its own recent performance.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrainState {
    /// The North Star is rising. Whatever the brain is doing is working.
    Improving,
    /// Flat, and the brain still has moves it has not made. It is looking for
    /// an edge, not failing to find one — the state a young system spends
    /// most of its life in, and the one most easily mistaken for a fault.
    Learning,
    /// Flat for long enough that the current approach has answered. The brain
    /// is not going to find it from here; it needs a source, a channel or an
    /// audience it does not currently have.
    Stagnant,
    /// The North Star is falling. Worth a human's attention before anything
    /// else on this list.
    Regressing,
    /// Not enough history to have an opinion.
    #[default]
    Initializing,
}

impl BrainState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Improving => "improving",
            Self::Learning => "learning",
            Self::Stagnant => "stagnant",
            Self::Regressing => "regressing",
            Self::Initializing => "initializing",
        }
    }

    /// True when a person should look at this now.
    ///
    /// `Learning` is deliberately not urgent. A brain that reports learning
    /// while the fan count is flat is describing a starved system, and calling
    /// that an alarm every five minutes is how an operator learns to ignore
    /// the alarm.
    #[must_use]
    pub const fn needs_attention(self) -> bool {
        matches!(self, Self::Regressing | Self::Stagnant)
    }

    /// Additive bonus to EFE exploration weight. When the brain is
    /// stagnant or regressing, it explores harder — seeking new edges.
    ///
    /// This enters **EFE weights**, not `DecisionValue.total()`. EFE
    /// decides what is worth learning about; DecisionValue decides
    /// what is worth doing.
    #[must_use]
    pub const fn exploration_boost(self) -> f64 {
        match self {
            Self::Improving => 0.0,    // working — no extra exploration needed
            Self::Learning => 0.5,     // looking for an edge
            Self::Stagnant => 0.3,     // current approach exhausted, explore
            Self::Regressing => 0.6,   // losing ground — explore urgently
            Self::Initializing => 0.4, // young system, explore to learn
        }
    }

    /// Multiplier for dispatch budget (0.0–1.0). When the brain is
    /// regressing, it reduces dispatch budget to limit damage.
    ///
    /// This enters **dispatch budget** (a constraint), not a value
    /// term. It scales how many dispatches a template gets, not how
    /// much each dispatch is worth.
    #[must_use]
    pub const fn sizing_multiplier(self) -> f64 {
        match self {
            Self::Improving => 1.0,    // full budget — it's working
            Self::Learning => 0.9,     // nearly full — still earning trust
            Self::Stagnant => 0.7,     // reduced — current approach is flat
            Self::Regressing => 0.5,   // half — limit damage while exploring
            Self::Initializing => 0.3, // minimal — young system, be careful
        }
    }
}

/// One day's North Star reading. `day` is a day number, so callers decide the
/// calendar; only the ordering and the gaps between values matter here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DailyNorthStar {
    pub day: i64,
    pub value: f64,
}

/// How long a flat North Star counts as learning before it counts as
/// stagnation.
///
/// Thirty days rather than Kern's fifty thousand cycles: those are seventeen
/// days at its cadence, and the same span reads better in the unit an operator
/// thinks in. A month of no movement is long enough to conclude the current
/// channels are exhausted, and short enough to still be worth acting on.
pub const STAGNATION_AFTER_FLAT_DAYS: usize = 30;

/// Assesses the trend in a series of daily North Star readings.
///
/// `samples` need not be sorted or unique per day; readings are collapsed to
/// one value per day, keeping the last, and ordered before comparison. That
/// matches how the data arrives — one row per brain cycle, many per day.
#[must_use]
pub fn assess(mut samples: Vec<DailyNorthStar>) -> BrainState {
    samples.sort_by_key(|sample| sample.day);
    samples.dedup_by_key(|sample| sample.day);
    if samples.len() < MINIMUM_DAYS {
        return BrainState::Initializing;
    }

    let half = samples.len() / 2;
    let earlier = mean(&samples[..half]);
    let later = mean(&samples[half..]);

    // A North Star of zero cannot be compared proportionally, and a system
    // with no fans at all is not improving or regressing — it has not started.
    if earlier <= 0.0 {
        return if later > 0.0 {
            BrainState::Improving
        } else {
            BrainState::Learning
        };
    }

    let change = (later - earlier) / earlier;
    if change > MATERIAL_CHANGE {
        BrainState::Improving
    } else if change < -MATERIAL_CHANGE {
        BrainState::Regressing
    } else if samples.len() >= STAGNATION_AFTER_FLAT_DAYS {
        BrainState::Stagnant
    } else {
        BrainState::Learning
    }
}

fn mean(samples: &[DailyNorthStar]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.iter().map(|sample| sample.value).sum::<f64>() / samples.len() as f64
}

/// Tracks the brain's self-assessment over time and provides behavior
/// outputs that feed back into the brain cycle.
///
/// Ported from Kern's `MetacognitionMonitor`. The monitor records each
/// assessment and tracks how long the brain has been in each state.
/// The `stagnation_escape_boost()` grows with learning cycles, driving
/// the brain to seek new edges harder the longer it goes without
/// improvement.
///
/// # DecisionValue invariant
///
/// `exploration_boost()` and `sizing_multiplier()` do NOT enter
/// `DecisionValue.total()`. They enter EFE weights (exploration) and
/// dispatch budget (sizing), which are separate from the value
/// calculation. This is the same separation Kern uses.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct MetacognitionMonitor {
    /// The current assessed state.
    pub state: BrainState,
    /// Consecutive cycles the brain has been in the Learning state.
    pub learning_cycles: u32,
    /// Consecutive cycles the brain has been Stagnant.
    pub stagnant_cycles: u32,
    /// Consecutive cycles the brain has been Regressing.
    pub regressing_cycles: u32,
    /// Total cycles the brain has been Improving (lifetime).
    pub improving_cycles_total: u32,
    /// Whether the monitor was reset after a prolonged halt.
    pub reset_after_halt: bool,
}

impl MetacognitionMonitor {
    /// Creates a new monitor in the Initializing state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a new assessment and updates cycle counters.
    pub fn observe(&mut self, state: BrainState) {
        if state == self.state {
            match state {
                BrainState::Learning => self.learning_cycles += 1,
                BrainState::Stagnant => self.stagnant_cycles += 1,
                BrainState::Regressing => self.regressing_cycles += 1,
                BrainState::Improving => self.improving_cycles_total += 1,
                BrainState::Initializing => {}
            }
        } else {
            // State changed — reset counters for the old state.
            match self.state {
                BrainState::Learning => self.learning_cycles = 0,
                BrainState::Stagnant => self.stagnant_cycles = 0,
                BrainState::Regressing => self.regressing_cycles = 0,
                BrainState::Improving | BrainState::Initializing => {}
            }
            // Set new counter to 1 (this is the first cycle in the new state).
            match state {
                BrainState::Learning => self.learning_cycles = 1,
                BrainState::Stagnant => self.stagnant_cycles = 1,
                BrainState::Regressing => self.regressing_cycles = 1,
                BrainState::Improving => self.improving_cycles_total += 1,
                BrainState::Initializing => {}
            }
        }
        self.state = state;
        self.reset_after_halt = false;
    }

    /// Returns the current exploration boost, including the stagnation
    /// escape bonus.
    ///
    /// The base boost comes from `BrainState::exploration_boost()`. The
    /// stagnation escape bonus grows with `learning_cycles`, driving the
    /// brain to seek new edges harder the longer it goes without
    /// improvement. The bonus saturates at 0.5 (doubled exploration).
    #[must_use]
    pub fn exploration_boost(&self) -> f64 {
        let base = self.state.exploration_boost();
        let escape = self.stagnation_escape_boost();
        base + escape
    }

    /// Returns the current sizing multiplier for dispatch budget.
    #[must_use]
    pub fn sizing_multiplier(&self) -> f64 {
        self.state.sizing_multiplier()
    }

    /// Progressive bonus that grows with learning_cycles, driving the
    /// brain to seek new edges harder the longer it goes without
    /// improvement. Saturates at 0.5.
    #[must_use]
    pub fn stagnation_escape_boost(&self) -> f64 {
        if self.learning_cycles == 0 {
            return 0.0;
        }
        // 0.02 per learning cycle, saturating at 0.5 (25 cycles).
        (0.02 * self.learning_cycles as f64).min(0.5)
    }

    /// Resets the monitor after a prolonged halt. Clears stale
    /// zero-alpha observations and returns to Initializing.
    pub fn reset_after_halt(&mut self) {
        self.state = BrainState::Initializing;
        self.learning_cycles = 0;
        self.stagnant_cycles = 0;
        self.regressing_cycles = 0;
        self.reset_after_halt = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(values: &[f64]) -> Vec<DailyNorthStar> {
        values
            .iter()
            .enumerate()
            .map(|(index, value)| DailyNorthStar {
                day: index as i64,
                value: *value,
            })
            .collect()
    }

    #[test]
    fn too_little_history_is_not_an_opinion() {
        assert_eq!(assess(series(&[10.0; 5])), BrainState::Initializing);
    }

    #[test]
    fn a_flat_north_star_is_learning_not_broken() {
        // The state CrowdRelay was in and could not say: ten fans, unchanged,
        // every cycle succeeding and nothing to show for it.
        assert_eq!(assess(series(&[10.0; 8])), BrainState::Learning);
    }

    #[test]
    fn a_month_of_flat_is_stagnation() {
        assert_eq!(
            assess(series(&[10.0; STAGNATION_AFTER_FLAT_DAYS])),
            BrainState::Stagnant
        );
    }

    #[test]
    fn growth_is_improving() {
        assert_eq!(
            assess(series(&[10.0, 10.0, 10.0, 40.0, 55.0, 70.0])),
            BrainState::Improving
        );
    }

    #[test]
    fn decline_is_regressing() {
        assert_eq!(
            assess(series(&[70.0, 65.0, 60.0, 20.0, 12.0, 10.0])),
            BrainState::Regressing
        );
    }

    #[test]
    fn the_threshold_is_proportional_not_absolute() {
        // One fan out of ten is a real move; one out of a thousand is not.
        // An absolute threshold would have to call both the same.
        assert_eq!(
            assess(series(&[10.0, 10.0, 10.0, 11.0, 11.0, 11.0])),
            BrainState::Improving
        );
        assert_eq!(
            assess(series(&[1000.0, 1000.0, 1000.0, 1001.0, 1001.0, 1001.0])),
            BrainState::Learning
        );
    }

    #[test]
    fn a_first_fan_counts_as_improvement() {
        assert_eq!(
            assess(series(&[0.0, 0.0, 0.0, 1.0, 2.0, 3.0])),
            BrainState::Improving
        );
    }

    #[test]
    fn an_empty_fanbase_has_not_started() {
        assert_eq!(assess(series(&[0.0; 8])), BrainState::Learning);
    }

    #[test]
    fn many_readings_in_one_day_are_one_observation() {
        // One row per cycle means hundreds per day. Without collapsing, a
        // single day of five-minute cycles would look like months of history
        // and the brain would claim a trend from one afternoon.
        let mut samples = Vec::new();
        for _ in 0..200 {
            samples.push(DailyNorthStar {
                day: 1,
                value: 10.0,
            });
        }
        assert_eq!(assess(samples), BrainState::Initializing);
    }

    #[test]
    fn only_regression_and_stagnation_ask_for_a_human() {
        assert!(BrainState::Regressing.needs_attention());
        assert!(BrainState::Stagnant.needs_attention());
        assert!(!BrainState::Learning.needs_attention());
        assert!(!BrainState::Improving.needs_attention());
        assert!(!BrainState::Initializing.needs_attention());
    }

    // ─── Behavior feedback tests ───

    #[test]
    fn exploration_boost_is_zero_when_improving() {
        assert_eq!(BrainState::Improving.exploration_boost(), 0.0);
    }

    #[test]
    fn exploration_boost_is_highest_when_regressing() {
        assert!(
            BrainState::Regressing.exploration_boost() > BrainState::Stagnant.exploration_boost()
        );
        assert!(BrainState::Stagnant.exploration_boost() > 0.0);
    }

    #[test]
    fn sizing_multiplier_is_full_when_improving() {
        assert_eq!(BrainState::Improving.sizing_multiplier(), 1.0);
    }

    #[test]
    fn sizing_multiplier_is_half_when_regressing() {
        assert_eq!(BrainState::Regressing.sizing_multiplier(), 0.5);
    }

    #[test]
    fn sizing_multiplier_is_minimal_when_initializing() {
        assert_eq!(BrainState::Initializing.sizing_multiplier(), 0.3);
    }

    // ─── MetacognitionMonitor tests ───

    #[test]
    fn monitor_starts_initializing() {
        let monitor = MetacognitionMonitor::new();
        assert_eq!(monitor.state, BrainState::Initializing);
        assert_eq!(monitor.learning_cycles, 0);
    }

    #[test]
    fn monitor_tracks_learning_cycles() {
        let mut monitor = MetacognitionMonitor::new();
        monitor.observe(BrainState::Learning);
        assert_eq!(monitor.learning_cycles, 1);
        monitor.observe(BrainState::Learning);
        assert_eq!(monitor.learning_cycles, 2);
    }

    #[test]
    fn monitor_resets_cycles_on_state_change() {
        let mut monitor = MetacognitionMonitor::new();
        for _ in 0..5 {
            monitor.observe(BrainState::Learning);
        }
        assert_eq!(monitor.learning_cycles, 5);
        monitor.observe(BrainState::Improving);
        assert_eq!(monitor.learning_cycles, 0);
        assert_eq!(monitor.state, BrainState::Improving);
    }

    #[test]
    fn monitor_stagnation_escape_grows_with_learning_cycles() {
        let mut monitor = MetacognitionMonitor::new();
        for _ in 0..10 {
            monitor.observe(BrainState::Learning);
        }
        let boost = monitor.stagnation_escape_boost();
        assert!(boost > 0.0);
        // 10 cycles * 0.02 = 0.2
        assert!((boost - 0.2).abs() < 0.01);
    }

    #[test]
    fn monitor_stagnation_escape_saturates() {
        let mut monitor = MetacognitionMonitor::new();
        for _ in 0..100 {
            monitor.observe(BrainState::Learning);
        }
        let boost = monitor.stagnation_escape_boost();
        assert!((boost - 0.5).abs() < 0.01); // saturated at 0.5
    }

    #[test]
    fn monitor_exploration_boost_includes_escape() {
        let mut monitor = MetacognitionMonitor::new();
        for _ in 0..10 {
            monitor.observe(BrainState::Learning);
        }
        // Base Learning boost = 0.5, escape = 0.2, total = 0.7
        let boost = monitor.exploration_boost();
        assert!((boost - 0.7).abs() < 0.01);
    }

    #[test]
    fn monitor_reset_after_halt_clears_state() {
        let mut monitor = MetacognitionMonitor::new();
        for _ in 0..10 {
            monitor.observe(BrainState::Learning);
        }
        assert_eq!(monitor.state, BrainState::Learning);
        monitor.reset_after_halt();
        assert_eq!(monitor.state, BrainState::Initializing);
        assert_eq!(monitor.learning_cycles, 0);
        assert!(monitor.reset_after_halt);
    }
}
