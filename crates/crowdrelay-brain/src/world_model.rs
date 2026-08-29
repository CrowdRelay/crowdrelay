//! World Model — the brain's belief about the world.
//!
//! One unified picture of everything the brain knows about the workspace's
//! fan acquisition state, with uncertainty. Every number is derived from
//! real data (fans, signal installs, community posts, outreach targets,
//! events). The brain uses this to decide what to do next.

use serde::{Deserialize, Serialize};

/// The brain's belief about the world — one unified picture with uncertainty.
/// Every number carries implicit confidence (the brain knows it has exact
/// counts for fans and signal installs, but averages for engagement).
///
/// This replaces the scattered per-template fields that were duplicated
/// across `GrowthIntelligenceSnapshot` instances. The world model is loaded
/// once per cycle and shared across all template evaluations.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WorldModel {
    // ── Fan aggregation state ──
    /// Total fans in the system.
    pub total_fans: u32,
    /// New fans added this month.
    pub fans_this_month: u32,
    /// Monthly fan growth rate in basis points (e.g. 500 = 5% monthly growth).
    pub fan_growth_rate_bps: u16,
    /// Whether fan growth is accelerating, steady, decelerating, or stagnant.
    pub fan_growth_trend: GrowthTrend,

    // ── Signal conversion state ──
    /// Total Signal push endpoints installed.
    pub total_signal_installs: u32,
    /// Signal installs this month.
    pub signal_installs_this_month: u32,
    /// Signal conversion rate: what fraction of fans have Signal installed
    /// (in basis points, e.g. 1000 = 10%).
    pub signal_conversion_rate_bps: u16,

    // ── Community reach state ──
    /// Total discovered communities (discovery_places with status='active').
    pub discovered_communities: u32,
    /// Communities with at least one post in the last 30 days.
    pub active_communities: u32,
    /// Average upvote ratio across all active communities (in basis points).
    pub avg_community_engagement_bps: u16,
    /// Best performing community by avg score, if any.
    pub best_performing_community: Option<String>,
    /// Worst performing community by avg score, if any.
    pub worst_performing_community: Option<String>,

    // ── Outreach pipeline state ──
    /// Outreach targets proposed but not yet promoted.
    pub pending_outreach_targets: u32,
    /// Outreach targets promoted but not yet engaged with community posts.
    pub promoted_outreach_targets: u32,
    /// Outreach targets that have community posts (engaged).
    pub engaged_outreach_targets: u32,

    // ── Event state ──
    /// Days until the nearest upcoming published event, or `None`.
    pub days_to_next_event: Option<u32>,
    /// Whether there is an upcoming event within 30 days.
    pub has_upcoming_event: bool,

    // ── Growth target progress ──
    /// How close the brain is to its fan acquisition target this month.
    pub growth_target_progress: GrowthTargetProgress,
}

/// The trend of fan growth over time. The brain uses this to decide
/// urgency: stagnant growth → more aggressive dispatch; accelerating →
/// maintain the current approach.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrowthTrend {
    /// Growth rate is increasing month-over-month.
    Accelerating,
    /// Growth rate is stable.
    #[default]
    Steady,
    /// Growth rate is decreasing.
    Decelerating,
    /// No new fans in the stagnation window.
    Stagnant,
}

impl GrowthTrend {
    /// Returns true if the brain should treat this as a stagnant situation
    /// — one that warrants more aggressive fan acquisition dispatch.
    #[must_use]
    pub const fn is_stagnant(self) -> bool {
        matches!(self, Self::Stagnant | Self::Decelerating)
    }

    /// Returns the string representation for use as a map key.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stagnant => "stagnant",
            Self::Decelerating => "decelerating",
            Self::Steady => "steady",
            Self::Accelerating => "accelerating",
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Growth Targets — the brain's monthly fan acquisition goals.
//
// Targets are derived deterministically from the current fan count:
// smaller fanbases get more aggressive targets (aggregation phase),
// larger ones get steadier targets (growth phase).
// ──────────────────────────────────────────────────────────────────────

/// The brain's monthly fan acquisition target.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct GrowthTarget {
    /// New fans to acquire this month.
    pub new_fans_per_month: u32,
    /// Signal installs to achieve this month.
    pub signal_installs_per_month: u32,
}

impl GrowthTarget {
    /// Derives a target from the current fan count. Smaller fanbases get
    /// more aggressive targets because the aggregation phase has more
    /// low-hanging fruit.
    #[must_use]
    pub fn from_fan_count(total_fans: u32) -> Self {
        let new_fans = match total_fans {
            0..=99 => 20,    // aggressive aggregation: 20 new fans/month
            100..=999 => 50, // growth phase: 50 new fans/month
            _ => 100,        // established: 100 new fans/month
        };
        // Signal installs target: 10% of fan count per month.
        let signal_installs = (total_fans / 10).max(5);
        Self {
            new_fans_per_month: new_fans,
            signal_installs_per_month: signal_installs,
        }
    }
}

/// How close the brain is to its growth target this month.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct GrowthTargetProgress {
    /// The target for this month.
    pub target: GrowthTarget,
    /// Fans acquired so far this month.
    pub fans_this_month: u32,
    /// Signal installs so far this month.
    pub signal_installs_this_month: u32,
    /// Progress toward the fan target in basis points (0–10_000).
    /// 10_000 = target met. Computed as `fans_this_month / target * 10_000`.
    pub progress_bps: u16,
    /// Whether the brain is behind, on track, or ahead of target.
    pub status: TargetStatus,
}

/// How the brain is doing relative to its growth target.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetStatus {
    /// Less than 50% of target pace — the brain needs to be more aggressive.
    Behind,
    /// 50–80% of target pace — the brain is making progress.
    #[default]
    OnTrack,
    /// More than 80% of target pace — the brain is succeeding.
    Ahead,
}

impl GrowthTargetProgress {
    /// Computes progress from a target and current counts.
    #[must_use]
    pub fn from_counts(
        target: GrowthTarget,
        fans_this_month: u32,
        signal_installs_this_month: u32,
    ) -> Self {
        let progress_bps = if target.new_fans_per_month == 0 {
            10_000
        } else {
            u16::try_from(
                (u64::from(fans_this_month) * 10_000 / u64::from(target.new_fans_per_month))
                    .min(10_000),
            )
            .unwrap_or(10_000)
        };
        let status = match progress_bps {
            0..=4_999 => TargetStatus::Behind,
            5_000..=7_999 => TargetStatus::OnTrack,
            _ => TargetStatus::Ahead,
        };
        Self {
            target,
            fans_this_month,
            signal_installs_this_month,
            progress_bps,
            status,
        }
    }
}

// ─── State-Transition World Model (P2.5) ─────────────────────────────────

/// A state-transition model that learns the probability of transitioning
/// between growth states (stagnant → accelerating, etc.) given actions taken.
///
/// The model tracks transition counts: `P(next_state | current_state, action_taken)`.
/// This lets the brain simulate state trajectories, not just fan counts.
///
/// The action is discretized as "dispatch" vs "no_dispatch" — the key
/// question is whether dispatching moves the system from stagnant to
/// accelerating.
#[allow(dead_code)] // TODO: wire into production path (next sprint)
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StateTransitionModel {
    /// Transition counts: key = "current_state:action:next_state" → count.
    transitions: std::collections::HashMap<String, u32>,
    /// Total counts per (current_state, action): key = "current_state:action" → count.
    totals: std::collections::HashMap<String, u32>,
}

#[allow(dead_code)] // TODO: wire into production path (next sprint)
impl StateTransitionModel {
    /// Creates a new state-transition model.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Computes the key for a (current_state, action, next_state) triple.
    fn transition_key(current: GrowthTrend, action: &str, next: GrowthTrend) -> String {
        format!("{}:{action}:{}", current.as_str(), next.as_str())
    }

    /// Computes the key for a (current_state, action) pair.
    fn total_key(current: GrowthTrend, action: &str) -> String {
        format!("{}:{action}", current.as_str())
    }

    /// Records a state transition: the system was in `current_state`, action
    /// `action` was taken, and the system transitioned to `next_state`.
    #[allow(dead_code)] // TODO: wire into production path (next sprint)
    pub fn update(&mut self, current: GrowthTrend, action: &str, next: GrowthTrend) {
        let tkey = Self::transition_key(current, action, next);
        let totkey = Self::total_key(current, action);
        *self.transitions.entry(tkey).or_insert(0) += 1;
        *self.totals.entry(totkey).or_insert(0) += 1;
    }

    /// Predicts the probability of transitioning to `next_state` given the
    /// current state and action. Returns 0.0 when no data is available.
    #[must_use]
    pub fn probability(&self, current: GrowthTrend, action: &str, next: GrowthTrend) -> f64 {
        let tkey = Self::transition_key(current, action, next);
        let totkey = Self::total_key(current, action);
        let count = self.transitions.get(&tkey).copied().unwrap_or(0);
        let total = self.totals.get(&totkey).copied().unwrap_or(0);
        if total == 0 {
            return 0.0;
        }
        f64::from(count) / f64::from(total)
    }

    /// Predicts the most likely next state given the current state and action.
    /// Returns `None` when no data is available.
    #[must_use]
    pub fn predict_transition(&self, current: GrowthTrend, action: &str) -> Option<GrowthTrend> {
        let mut best: Option<(GrowthTrend, f64)> = None;
        for next in [
            GrowthTrend::Stagnant,
            GrowthTrend::Decelerating,
            GrowthTrend::Steady,
            GrowthTrend::Accelerating,
        ] {
            let p = self.probability(current, action, next);
            if p > 0.0 && (best.is_none() || p > best.unwrap().1) {
                best = Some((next, p));
            }
        }
        best.map(|(s, _)| s)
    }

    /// Returns the confidence (total observation count) for a (state, action) pair.
    #[must_use]
    pub fn confidence(&self, current: GrowthTrend, action: &str) -> u32 {
        let key = Self::total_key(current, action);
        self.totals.get(&key).copied().unwrap_or(0)
    }
}

// ──────────────────────────────────────────────────────────────────────
// Rich State-Transition Model (P1.5)
//
// The simple StateTransitionModel above only uses GrowthTrend as the state.
// The RichStateTransitionModel uses a multi-dimensional state that captures
// the brain's full situation: growth trend, fanbase size tier, target
// progress, and event proximity. This lets the brain learn more nuanced
// transition probabilities (e.g. "when the fanbase is small and stagnant
// and an event is close, dispatching is more likely to accelerate growth").
// ──────────────────────────────────────────────────────────────────────

/// A rich, multi-dimensional state descriptor for the world model.
///
/// This captures the brain's full situation at a decision point, enabling
/// more nuanced strategy conditioning (P1.3) and state-transition learning
/// (P1.5) than the simple `GrowthTrend` alone.
#[allow(dead_code)] // TODO: wire into production path (next sprint)
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct RichState {
    /// Growth trend (stagnant, steady, decelerating, accelerating).
    pub growth_trend: GrowthTrend,
    /// Fanbase size tier.
    pub fanbase_tier: FanbaseTier,
    /// Target progress (how close to the monthly target).
    pub target_progress: TargetProgress,
    /// Event proximity.
    pub event_proximity: EventProximity,
}

impl RichState {
    /// Computes a string key for HashMap storage.
    #[must_use]
    pub fn key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.growth_trend.as_str(),
            self.fanbase_tier.as_str(),
            self.target_progress.as_str(),
            self.event_proximity.as_str()
        )
    }
}

/// Fanbase size tier — discretized for state conditioning.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum FanbaseTier {
    /// 0-100 fans (just starting).
    Seedling,
    /// 100-1000 fans (growing).
    #[default]
    Emerging,
    /// 1000-10000 fans (established).
    Established,
    /// 10000+ fans (large).
    Large,
}

impl FanbaseTier {
    /// Classifies a fan count into a tier.
    #[must_use]
    pub fn from_fan_count(fans: u32) -> Self {
        match fans {
            0..=100 => Self::Seedling,
            101..=1000 => Self::Emerging,
            1001..=10000 => Self::Established,
            _ => Self::Large,
        }
    }

    /// Returns the string representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Seedling => "seedling",
            Self::Emerging => "emerging",
            Self::Established => "established",
            Self::Large => "large",
        }
    }
}

/// Target progress — how close the brain is to the monthly target.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum TargetProgress {
    /// Behind schedule (<50% of target).
    Behind,
    /// On track (50-100% of target).
    #[default]
    OnTrack,
    /// Ahead of schedule (>100% of target).
    Ahead,
}

impl TargetProgress {
    /// Classifies a progress ratio into a tier.
    #[must_use]
    pub fn from_ratio(actual: f64, target: f64) -> Self {
        if target <= 0.0 {
            return Self::OnTrack;
        }
        let ratio = actual / target;
        if ratio < 0.5 {
            Self::Behind
        } else if ratio > 1.0 {
            Self::Ahead
        } else {
            Self::OnTrack
        }
    }

    /// Returns the string representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Behind => "behind",
            Self::OnTrack => "on_track",
            Self::Ahead => "ahead",
        }
    }
}

/// Event proximity — how close the next event is.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum EventProximity {
    /// No event scheduled.
    #[default]
    None,
    /// Event is far (>30 days).
    Far,
    /// Event is near (7-30 days).
    Near,
    /// Event is close (≤7 days).
    Close,
}

impl EventProximity {
    /// Classifies days-to-event into a proximity tier.
    #[must_use]
    pub fn from_days(days: Option<i64>) -> Self {
        match days {
            None => Self::None,
            Some(d) if d <= 7 => Self::Close,
            Some(d) if d <= 30 => Self::Near,
            Some(_) => Self::Far,
        }
    }

    /// Returns the string representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "no_event",
            Self::Far => "far",
            Self::Near => "near",
            Self::Close => "close",
        }
    }
}

/// A rich state-transition model that learns transition probabilities
/// conditioned on the full `RichState`, not just `GrowthTrend`.
///
/// This lets the brain learn nuanced patterns like "when the fanbase is
/// small and stagnant and an event is close, dispatching is more likely to
/// accelerate growth than when the fanbase is large and ahead of target."
#[allow(dead_code)] // TODO: wire into production path (next sprint)
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RichStateTransitionModel {
    /// Transition counts: key = "state_key:action:next_trend" → count.
    transitions: std::collections::HashMap<String, u32>,
    /// Total counts per (state, action): key = "state_key:action" → count.
    totals: std::collections::HashMap<String, u32>,
}

#[allow(dead_code)] // TODO: wire into production path (next sprint)
impl RichStateTransitionModel {
    /// Creates a new rich state-transition model.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a state transition.
    pub fn update(&mut self, state: &RichState, action: &str, next: GrowthTrend) {
        let tkey = format!("{}:{action}:{}", state.key(), next.as_str());
        let totkey = format!("{}:{action}", state.key());
        *self.transitions.entry(tkey).or_insert(0) += 1;
        *self.totals.entry(totkey).or_insert(0) += 1;
    }

    /// Predicts the probability of transitioning to `next` given the state
    /// and action. Returns 0.0 when no data is available.
    #[must_use]
    pub fn probability(&self, state: &RichState, action: &str, next: GrowthTrend) -> f64 {
        let tkey = format!("{}:{action}:{}", state.key(), next.as_str());
        let totkey = format!("{}:{action}", state.key());
        let count = self.transitions.get(&tkey).copied().unwrap_or(0);
        let total = self.totals.get(&totkey).copied().unwrap_or(0);
        if total == 0 {
            return 0.0;
        }
        f64::from(count) / f64::from(total)
    }

    /// Predicts the most likely next trend given the state and action.
    #[must_use]
    pub fn predict_transition(&self, state: &RichState, action: &str) -> Option<GrowthTrend> {
        let mut best: Option<(GrowthTrend, f64)> = None;
        for next in [
            GrowthTrend::Stagnant,
            GrowthTrend::Decelerating,
            GrowthTrend::Steady,
            GrowthTrend::Accelerating,
        ] {
            let p = self.probability(state, action, next);
            if p > 0.0 && (best.is_none() || p > best.unwrap().1) {
                best = Some((next, p));
            }
        }
        best.map(|(s, _)| s)
    }

    /// Returns the confidence (total observations) for a (state, action) pair.
    #[must_use]
    pub fn confidence(&self, state: &RichState, action: &str) -> u32 {
        let key = format!("{}:{action}", state.key());
        self.totals.get(&key).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn growth_target_for_small_fanbase_is_aggressive() {
        let target = GrowthTarget::from_fan_count(50);
        assert_eq!(target.new_fans_per_month, 20);
        assert_eq!(target.signal_installs_per_month, 5);
    }

    #[test]
    fn growth_target_for_medium_fanbase_is_moderate() {
        let target = GrowthTarget::from_fan_count(500);
        assert_eq!(target.new_fans_per_month, 50);
        assert_eq!(target.signal_installs_per_month, 50);
    }

    #[test]
    fn growth_target_for_large_fanbase_is_steady() {
        let target = GrowthTarget::from_fan_count(5000);
        assert_eq!(target.new_fans_per_month, 100);
        assert_eq!(target.signal_installs_per_month, 500);
    }

    #[test]
    fn target_progress_behind_when_far_from_target() {
        let target = GrowthTarget {
            new_fans_per_month: 20,
            signal_installs_per_month: 5,
        };
        let progress = GrowthTargetProgress::from_counts(target, 5, 1);
        assert_eq!(progress.progress_bps, 2_500); // 5/20 = 25%
        assert_eq!(progress.status, TargetStatus::Behind);
    }

    #[test]
    fn target_progress_on_track_when_halfway() {
        let target = GrowthTarget {
            new_fans_per_month: 20,
            signal_installs_per_month: 5,
        };
        let progress = GrowthTargetProgress::from_counts(target, 12, 3);
        assert_eq!(progress.progress_bps, 6_000); // 12/20 = 60%
        assert_eq!(progress.status, TargetStatus::OnTrack);
    }

    #[test]
    fn target_progress_ahead_when_near_or_above_target() {
        let target = GrowthTarget {
            new_fans_per_month: 20,
            signal_installs_per_month: 5,
        };
        let progress = GrowthTargetProgress::from_counts(target, 18, 4);
        assert_eq!(progress.progress_bps, 9_000); // 18/20 = 90%
        assert_eq!(progress.status, TargetStatus::Ahead);
    }

    #[test]
    fn target_progress_caps_at_10k_when_exceeding_target() {
        let target = GrowthTarget {
            new_fans_per_month: 20,
            signal_installs_per_month: 5,
        };
        let progress = GrowthTargetProgress::from_counts(target, 50, 10);
        assert_eq!(progress.progress_bps, 10_000); // capped
        assert_eq!(progress.status, TargetStatus::Ahead);
    }

    #[test]
    fn stagnant_trend_is_urgent() {
        assert!(GrowthTrend::Stagnant.is_stagnant());
        assert!(GrowthTrend::Decelerating.is_stagnant());
        assert!(!GrowthTrend::Steady.is_stagnant());
        assert!(!GrowthTrend::Accelerating.is_stagnant());
    }

    #[test]
    fn zero_fan_target_does_not_divide_by_zero() {
        let target = GrowthTarget {
            new_fans_per_month: 0,
            signal_installs_per_month: 0,
        };
        let progress = GrowthTargetProgress::from_counts(target, 5, 1);
        assert_eq!(progress.progress_bps, 10_000); // target met (no target)
        assert_eq!(progress.status, TargetStatus::Ahead);
    }

    // ── StateTransitionModel tests ───────────────────────────────────────

    #[test]
    fn state_transition_starts_empty() {
        let model = StateTransitionModel::new();
        assert_eq!(model.confidence(GrowthTrend::Stagnant, "dispatch"), 0);
        assert_eq!(
            model.probability(GrowthTrend::Stagnant, "dispatch", GrowthTrend::Accelerating),
            0.0
        );
        assert_eq!(
            model.predict_transition(GrowthTrend::Stagnant, "dispatch"),
            None
        );
    }

    #[test]
    fn state_transition_learns_probabilities() {
        let mut model = StateTransitionModel::new();
        // Dispatch from stagnant → accelerating 7 out of 10 times
        for _ in 0..7 {
            model.update(GrowthTrend::Stagnant, "dispatch", GrowthTrend::Accelerating);
        }
        for _ in 0..3 {
            model.update(GrowthTrend::Stagnant, "dispatch", GrowthTrend::Steady);
        }
        let p_accel =
            model.probability(GrowthTrend::Stagnant, "dispatch", GrowthTrend::Accelerating);
        let p_steady = model.probability(GrowthTrend::Stagnant, "dispatch", GrowthTrend::Steady);
        assert!(
            (p_accel - 0.7).abs() < 0.01,
            "P(accelerating) should be 0.7, got {p_accel}"
        );
        assert!(
            (p_steady - 0.3).abs() < 0.01,
            "P(steady) should be 0.3, got {p_steady}"
        );
        assert_eq!(model.confidence(GrowthTrend::Stagnant, "dispatch"), 10);
    }

    #[test]
    fn state_transition_predicts_most_likely() {
        let mut model = StateTransitionModel::new();
        for _ in 0..8 {
            model.update(GrowthTrend::Stagnant, "dispatch", GrowthTrend::Accelerating);
        }
        for _ in 0..2 {
            model.update(GrowthTrend::Stagnant, "dispatch", GrowthTrend::Steady);
        }
        let predicted = model.predict_transition(GrowthTrend::Stagnant, "dispatch");
        assert_eq!(predicted, Some(GrowthTrend::Accelerating));
    }

    #[test]
    fn state_transition_separates_actions() {
        let mut model = StateTransitionModel::new();
        // Dispatch → accelerating
        for _ in 0..10 {
            model.update(GrowthTrend::Stagnant, "dispatch", GrowthTrend::Accelerating);
        }
        // No dispatch → stays stagnant
        for _ in 0..10 {
            model.update(GrowthTrend::Stagnant, "no_dispatch", GrowthTrend::Stagnant);
        }
        let p_dispatch =
            model.probability(GrowthTrend::Stagnant, "dispatch", GrowthTrend::Accelerating);
        let p_no_dispatch = model.probability(
            GrowthTrend::Stagnant,
            "no_dispatch",
            GrowthTrend::Accelerating,
        );
        assert!(
            p_dispatch > 0.9,
            "dispatch should lead to accelerating, got {p_dispatch}"
        );
        assert!(
            p_no_dispatch < 0.1,
            "no_dispatch should not lead to accelerating, got {p_no_dispatch}"
        );
    }

    // ── Rich state-transition model tests (P1.5) ─────────────────────────

    #[test]
    fn fanbase_tier_classification() {
        assert_eq!(FanbaseTier::from_fan_count(50), FanbaseTier::Seedling);
        assert_eq!(FanbaseTier::from_fan_count(500), FanbaseTier::Emerging);
        assert_eq!(FanbaseTier::from_fan_count(5000), FanbaseTier::Established);
        assert_eq!(FanbaseTier::from_fan_count(50000), FanbaseTier::Large);
    }

    #[test]
    fn target_progress_classification() {
        assert_eq!(
            TargetProgress::from_ratio(10.0, 100.0),
            TargetProgress::Behind
        );
        assert_eq!(
            TargetProgress::from_ratio(60.0, 100.0),
            TargetProgress::OnTrack
        );
        assert_eq!(
            TargetProgress::from_ratio(120.0, 100.0),
            TargetProgress::Ahead
        );
    }

    #[test]
    fn event_proximity_classification() {
        assert_eq!(EventProximity::from_days(None), EventProximity::None);
        assert_eq!(EventProximity::from_days(Some(3)), EventProximity::Close);
        assert_eq!(EventProximity::from_days(Some(20)), EventProximity::Near);
        assert_eq!(EventProximity::from_days(Some(60)), EventProximity::Far);
    }

    #[test]
    fn rich_state_key_is_deterministic() {
        let s1 = RichState {
            growth_trend: GrowthTrend::Stagnant,
            fanbase_tier: FanbaseTier::Seedling,
            target_progress: TargetProgress::Behind,
            event_proximity: EventProximity::Close,
        };
        let s2 = RichState {
            growth_trend: GrowthTrend::Stagnant,
            fanbase_tier: FanbaseTier::Seedling,
            target_progress: TargetProgress::Behind,
            event_proximity: EventProximity::Close,
        };
        assert_eq!(s1.key(), s2.key());
    }

    #[test]
    fn rich_state_transition_learns_probabilities() {
        let mut model = RichStateTransitionModel::new();
        let state = RichState {
            growth_trend: GrowthTrend::Stagnant,
            fanbase_tier: FanbaseTier::Seedling,
            target_progress: TargetProgress::Behind,
            event_proximity: EventProximity::Close,
        };
        // Dispatch → accelerating 8 times, dispatch → stagnant 2 times.
        for _ in 0..8 {
            model.update(&state, "dispatch", GrowthTrend::Accelerating);
        }
        for _ in 0..2 {
            model.update(&state, "dispatch", GrowthTrend::Stagnant);
        }
        let p_accel = model.probability(&state, "dispatch", GrowthTrend::Accelerating);
        assert!(
            (p_accel - 0.8).abs() < 0.01,
            "P(accelerating|dispatch) should be 0.8, got {p_accel}"
        );
    }

    #[test]
    fn rich_state_transition_separates_states() {
        let mut model = RichStateTransitionModel::new();
        let state_small = RichState {
            growth_trend: GrowthTrend::Stagnant,
            fanbase_tier: FanbaseTier::Seedling,
            target_progress: TargetProgress::Behind,
            event_proximity: EventProximity::Close,
        };
        let state_large = RichState {
            growth_trend: GrowthTrend::Stagnant,
            fanbase_tier: FanbaseTier::Large,
            target_progress: TargetProgress::Ahead,
            event_proximity: EventProximity::None,
        };
        // Small fanbase: dispatch → accelerating.
        for _ in 0..10 {
            model.update(&state_small, "dispatch", GrowthTrend::Accelerating);
        }
        // Large fanbase: dispatch → steady (saturated).
        for _ in 0..10 {
            model.update(&state_large, "dispatch", GrowthTrend::Steady);
        }
        // The predictions should differ.
        let small_pred = model.predict_transition(&state_small, "dispatch");
        let large_pred = model.predict_transition(&state_large, "dispatch");
        assert_eq!(small_pred, Some(GrowthTrend::Accelerating));
        assert_eq!(large_pred, Some(GrowthTrend::Steady));
    }
}
