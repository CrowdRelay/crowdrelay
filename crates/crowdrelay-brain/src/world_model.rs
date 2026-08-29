//! World Model — the brain's belief about the world.
//!
//! One unified picture of everything the brain knows about the workspace's
//! fan acquisition state, with uncertainty. Every number is derived from
//! real data (fans, signal installs, community posts, outreach targets,
//! events). The brain uses this to decide what to do next.

use serde::Serialize;

/// The brain's belief about the world — one unified picture with uncertainty.
/// Every number carries implicit confidence (the brain knows it has exact
/// counts for fans and signal installs, but averages for engagement).
///
/// This replaces the scattered per-template fields that were duplicated
/// across `GrowthIntelligenceSnapshot` instances. The world model is loaded
/// once per cycle and shared across all template evaluations.
#[derive(Clone, Debug, Default, Serialize)]
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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, serde::Deserialize)]
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
}

// ──────────────────────────────────────────────────────────────────────
// Growth Targets — the brain's monthly fan acquisition goals.
//
// Targets are derived deterministically from the current fan count:
// smaller fanbases get more aggressive targets (aggregation phase),
// larger ones get steadier targets (growth phase).
// ──────────────────────────────────────────────────────────────────────

/// The brain's monthly fan acquisition target.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
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
}
