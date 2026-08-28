//! Deterministic growth intelligence policy.
//!
//! The brain is deterministic Rust machinery. This module holds the policy
//! knobs that decide when the brain dispatches LLM workers to gather
//! intelligence. The brain never follows an LLM blindly — it applies these
//! rules and decides what to gather, when, and what to do with it.
//!
//! LLMs are workers/tools/slaves that gather intelligence and draft content.
//! They do NOT decide strategy. The brain decides.

use serde::{Deserialize, Serialize};

/// Intelligent token optimization tier. The brain classifies each dispatched
/// task based on stakes and complexity:
///
/// - `Basic`: free-tier models handle volume (scan, draft, suggest)
/// - `Premium`: connected paid providers handle stakes (human contact, complex
///   analysis, strategic planning)
///
/// If no premium credential is connected, premium tasks silently fall back to
/// basic — the system never blocks, it degrades.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentTier {
    #[default]
    Basic,
    Premium,
}

impl AgentTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Premium => "premium",
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Agent worker standing — the learning loop for dispatched workers.
//
// Mirrors the play learning discipline in `learning.rs`: a worker that
// consistently produces no fan growth gets a longer cooldown, and one that
// consistently does gets a shorter one. A worker that worsens (produces
// negative growth) repeatedly is retired until an operator reinstates it.
//
// The same four rules apply:
// 1. An unmeasurable outcome is not a bad outcome.
// 2. One result changes nothing.
// 3. A record may only ever narrow (longer cooldown), never widen below base.
// 4. Retirement is a stated fact, not a decayed weight.
// ──────────────────────────────────────────────────────────────────────

/// What the measured record says about one worker template.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "standing", rename_all = "snake_case")]
pub enum AgentStanding {
    /// Too few measured dispatches to say anything. Runs at base cadence:
    /// the alternative is a brain that throttles every new worker before it
    /// has had a chance to produce growth.
    Untested { measured: u32 },
    /// Weighted by its own record. `10_000` is full effectiveness (base
    /// cadence); lower means the worker's cooldown is proportionally longer
    /// until the record improves.
    Weighted {
        effectiveness_bps: u16,
        measured: u32,
    },
    /// Dispatched no longer, until an operator reinstates it.
    Retired { reason: AgentRetirementReason },
}

impl AgentStanding {
    /// `10_000` when nothing is holding the worker back, and zero when it is
    /// retired.
    #[must_use]
    pub const fn effectiveness_bps(self) -> u16 {
        match self {
            Self::Untested { .. } => 10_000,
            Self::Weighted {
                effectiveness_bps, ..
            } => effectiveness_bps,
            Self::Retired { .. } => 0,
        }
    }

    #[must_use]
    pub const fn is_retired(self) -> bool {
        matches!(self, Self::Retired { .. })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRetirementReason {
    /// A run of measured dispatches that each produced no fan growth or
    /// worsened it.
    RepeatedlyIneffective,
    /// A human switched it off. Recorded separately so the brain never claims
    /// an operator's decision as its own conclusion.
    OperatorRetired,
}

impl AgentRetirementReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RepeatedlyIneffective => "repeatedly_ineffective",
            Self::OperatorRetired => "operator_retired",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "repeatedly_ineffective" => Some(Self::RepeatedlyIneffective),
            "operator_retired" => Some(Self::OperatorRetired),
            _ => None,
        }
    }
}

/// The record one worker template has accumulated from measured dispatches.
///
/// Counts rather than a score, because a score cannot be argued with. An
/// operator who disagrees with a standing can see exactly which dispatches
/// produced it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentRecord {
    pub improved: u32,
    pub neutral: u32,
    pub worsened: u32,
    /// Measured `worsened` outcomes since the last one that was not. Reset by
    /// any measured result that is not `worsened`.
    pub consecutive_worsened: u32,
    /// Set only by an operator. The brain never writes this.
    pub operator_retired: bool,
}

impl AgentRecord {
    /// Outcomes that actually said something.
    #[must_use]
    pub const fn measured(self) -> u32 {
        self.improved
            .saturating_add(self.neutral)
            .saturating_add(self.worsened)
    }
}

/// Policy for turning an `AgentRecord` into an `AgentStanding`. Mirrors
/// `LearningPolicy` but with defaults tuned for the slower cadence of agent
/// dispatch measurement (14-day windows vs 7-day play windows).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct AgentLearningPolicy {
    /// Measured dispatches required before the record moves anything at all.
    pub minimum_measured_record: u32,
    /// However bad the record, a worker that is still running keeps at least
    /// this share of its base cadence. A weight that could fall to nothing
    /// would be a silent retirement.
    pub floor_effectiveness_bps: u16,
    /// Consecutive measured `worsened` outcomes before the worker retires
    /// itself.
    pub retire_after_consecutive_worsened: u32,
}

impl Default for AgentLearningPolicy {
    fn default() -> Self {
        Self {
            // Two is lower than the play policy (3) because agent dispatches
            // are measured over 14-day windows — waiting for three full
            // measurements means six weeks before the brain can adapt.
            minimum_measured_record: 2,
            floor_effectiveness_bps: 2_000,
            retire_after_consecutive_worsened: 3,
        }
    }
}

/// Turns a record into a standing. Same discipline as `assess_play_standing`.
#[must_use]
pub fn assess_agent_standing(record: AgentRecord, policy: AgentLearningPolicy) -> AgentStanding {
    if record.operator_retired {
        return AgentStanding::Retired {
            reason: AgentRetirementReason::OperatorRetired,
        };
    }
    if record.consecutive_worsened >= policy.retire_after_consecutive_worsened.max(1) {
        return AgentStanding::Retired {
            reason: AgentRetirementReason::RepeatedlyIneffective,
        };
    }
    let measured = record.measured();
    if measured < policy.minimum_measured_record.max(1) {
        return AgentStanding::Untested { measured };
    }
    // Neutral counts as half. A worker that reliably produces no growth is
    // not as good as one that does and not as bad as one that harms.
    let credit = u64::from(record.improved)
        .saturating_mul(2)
        .saturating_add(u64::from(record.neutral));
    let basis_points = credit.saturating_mul(10_000) / u64::from(measured).max(1) / 2;
    let basis_points = u16::try_from(basis_points.min(10_000)).unwrap_or(10_000);
    AgentStanding::Weighted {
        effectiveness_bps: basis_points.max(policy.floor_effectiveness_bps.min(10_000)),
        measured,
    }
}

/// Computes the effective cooldown for a worker template given its standing.
///
/// Higher effectiveness → shorter cooldown (dispatch more often).
/// Lower effectiveness → longer cooldown (dispatch less often).
/// Retired → never dispatch (u32::MAX).
///
/// The adjustment is bounded: at most 4x the base cooldown, so a worker
/// with a poor record doesn't get pushed out to months between dispatches.
#[must_use]
pub fn effective_agent_cooldown(base_cooldown_hours: u32, standing: AgentStanding) -> u32 {
    match standing {
        AgentStanding::Untested { .. } => base_cooldown_hours,
        AgentStanding::Weighted {
            effectiveness_bps, ..
        } => {
            if effectiveness_bps == 0 {
                return base_cooldown_hours.saturating_mul(4);
            }
            // Scale: 10_000 bps → base cooldown, 2_000 bps → 5x base (capped at 4x).
            // The formula: base * (10_000 / effectiveness), capped at 4x.
            let factor = 10_000_u32 / effectiveness_bps.max(1) as u32;
            base_cooldown_hours
                .saturating_mul(factor)
                .min(base_cooldown_hours.saturating_mul(4))
        }
        AgentStanding::Retired { .. } => u32::MAX,
    }
}

/// Computes the effective tier for a worker dispatch given its standing.
///
/// A worker with consistently high effectiveness (>= 8_000 bps) escalates
/// to premium models — the situation is working and warrants a more
/// powerful model to maximize the proven growth channel.
#[must_use]
pub const fn effective_agent_tier(base_tier: AgentTier, standing: AgentStanding) -> AgentTier {
    match standing {
        AgentStanding::Weighted {
            effectiveness_bps, ..
        } if effectiveness_bps >= 8_000 => AgentTier::Premium,
        _ => base_tier,
    }
}

/// A recent unconsumed insight from an agent outcome. The brain reads these
/// before dispatching the next worker run and includes them in the dispatch
/// prompt so the worker knows what was already discovered. After the brain
/// factors an insight into its planning, it marks the row as consumed.
#[derive(Clone, Debug, Serialize)]
pub struct RecentInsight {
    /// The `agent_outcomes.id` — used to mark the row consumed after planning.
    pub outcome_id: uuid::Uuid,
    /// Which template produced this insight (derived from the task).
    pub template_id: String,
    /// The outcome kind: `campaign_insight`, `generic_insight`, `release_plan_note`.
    pub kind: String,
    /// A short headline from the insight payload, for inclusion in the prompt.
    pub headline: String,
    /// The detail/body of the insight, for inclusion in the prompt.
    pub detail: String,
    /// The recommended action, if any.
    pub recommended_action: Option<String>,
}

/// A snapshot of one worker template's dispatch state: when it last ran and
/// whether the workspace's current situation warrants a new dispatch. The
/// infra layer computes this from agent_service_tasks history and workspace
/// state; the deterministic evaluator consumes it.
#[derive(Clone, Debug, Serialize)]
pub struct GrowthIntelligenceSnapshot {
    /// The worker template ID, e.g. "reddit-scanner", "press-pitch".
    pub template_id: String,
    /// Hours since the last agent run for this template, or `None` if never run.
    /// This counts **any** run, including ones that produced zero items. Used
    /// for the failed-run retry delay so the brain doesn't retry every cycle.
    pub hours_since_last_run: Option<u32>,
    /// Hours since the last **effective** run — one that produced an outcome
    /// with a non-empty `items` array, or `None` if no effective run exists.
    /// The cooldown is measured from this, so a failed/empty run does not
    /// reset the cooldown. If this is `None`, the brain treats the cooldown
    /// as elapsed (never had a successful run → dispatch immediately).
    pub hours_since_last_effective_run: Option<u32>,
    /// Whether there is an upcoming event within the press-pitch lead window.
    pub has_upcoming_event: bool,
    /// Days until the nearest upcoming event, or `None`.
    pub days_to_next_event: Option<u32>,
    /// Whether fan growth has been stagnant for the configured period.
    pub fan_growth_stagnant: bool,
    /// Number of unengaged outreach targets (accepted but not yet engaged).
    pub unengaged_outreach_targets: u32,
    /// The actual unengaged outreach targets (id, display name, subreddit)
    /// for the community-engager prompt. Only populated for the
    /// `community-engager` template snapshot. The brain feeds these into
    /// the dispatch prompt so the LLM can produce concrete social_post
    /// outcomes with `target_id` and `subreddit` fields.
    pub unengaged_targets: Vec<UnengagedTarget>,
    /// Unconsumed insights from recent worker runs, keyed by template_id.
    /// The brain feeds these into the next dispatch prompt and marks them
    /// consumed after planning. This closes the feedback loop: workers
    /// produce insights → brain reads them → brain feeds them forward →
    /// brain marks them consumed → retention deletes after 7 days.
    pub recent_insights: Vec<RecentInsight>,
    /// Latest community engagement performance per subreddit, from
    /// `community_post_metrics`. Only populated for the `community-engager`
    /// template snapshot. The brain uses this to avoid dispatching to
    /// subreddits with consistently poor engagement and to include
    /// performance context in the worker prompt.
    pub community_engagement_history: Vec<CommunityEngagementSummary>,
    /// The measured standing of this worker template from past dispatch
    /// outcomes. The brain uses this to adjust the dispatch cadence (effective
    /// workers get shorter cooldowns, ineffective ones get longer ones) and
    /// to retire workers that consistently produce no fan growth.
    pub standing: AgentStanding,
}

/// A single unengaged outreach target that the community-engager should
/// draft a post for. Carries the concrete `target_id` and `subreddit` the
/// LLM needs to produce a `social_post` outcome with a valid
/// `community.engage.request` action.
#[derive(Clone, Debug, Serialize)]
pub struct UnengagedTarget {
    /// The `agent_outreach_targets.id` — becomes `target_id` in the
    /// social_post outcome item.
    pub target_id: uuid::Uuid,
    /// Human-readable name, e.g. "r/MetalPoland".
    pub display_name: String,
    /// Clean subreddit name without `r/` prefix, e.g. "MetalPoland".
    pub subreddit: String,
}

/// Aggregated performance of a single subreddit's recent community posts.
/// Derived from the latest `community_post_metrics` row per post, averaged
/// across all posts to that subreddit in the last 30 days.
#[derive(Clone, Debug, Serialize)]
pub struct CommunityEngagementSummary {
    /// The subreddit name (without `r/` prefix).
    pub subreddit: String,
    /// Number of posts to this subreddit in the window.
    pub post_count: u32,
    /// Average score across posts (Reddit's hotness ranking score).
    pub avg_score: f64,
    /// Average upvotes across posts.
    pub avg_upvotes: f64,
    /// Average comment count across posts.
    pub avg_comments: f64,
    /// Average upvote ratio across posts (0.0–1.0), if available.
    pub avg_upvote_ratio: Option<f64>,
}

/// Cooldown intervals (in hours) for each worker template. The brain will
/// not dispatch the same worker template more often than its cooldown.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct GrowthIntelligencePolicy {
    /// Hours between reddit-scanner dispatches. Default: 7 days.
    pub reddit_scanner_cooldown_hours: u32,
    /// Hours between community-engager dispatches. Default: 5 days.
    pub community_engager_cooldown_hours: u32,
    /// Hours between press-pitch dispatches. Default: 3 days.
    pub press_pitch_cooldown_hours: u32,
    /// Hours between social-post dispatches. Default: 2 days.
    pub social_post_cooldown_hours: u32,
    /// Hours between signal-inviter dispatches. Default: 7 days.
    pub signal_inviter_cooldown_hours: u32,
    /// Hours between growth-strategist (intelligence analyst) dispatches. Default: 1 day.
    pub growth_strategist_cooldown_hours: u32,
    /// Days before an event to start press outreach. Default: 30 days.
    pub press_pitch_event_lead_days: u32,
    /// Days of stagnant fan growth before dispatching community engagement. Default: 14 days.
    pub fan_growth_stagnant_days: u32,
    /// Minimum hours to wait before retrying a worker after a failed/empty
    /// run. Prevents retry storms on the 5-minute autopilot cycle when a
    /// worker keeps producing zero items. The hard cap (`max_actions_24h`
    /// in the autopilot policy table) is the ultimate backstop.
    /// Default: 1 hour.
    pub failed_run_retry_hours: u32,
}

impl Default for GrowthIntelligencePolicy {
    fn default() -> Self {
        Self {
            reddit_scanner_cooldown_hours: 168,    // 7 days
            community_engager_cooldown_hours: 120, // 5 days
            press_pitch_cooldown_hours: 72,        // 3 days
            social_post_cooldown_hours: 48,        // 2 days
            signal_inviter_cooldown_hours: 168,    // 7 days
            growth_strategist_cooldown_hours: 24,  // 1 day
            press_pitch_event_lead_days: 30,
            fan_growth_stagnant_days: 14,
            failed_run_retry_hours: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> AgentLearningPolicy {
        AgentLearningPolicy::default()
    }

    #[test]
    fn untested_worker_runs_at_base_cadence() {
        let record = AgentRecord::default();
        let standing = assess_agent_standing(record, policy());
        assert!(matches!(standing, AgentStanding::Untested { measured: 0 }));
        assert_eq!(effective_agent_cooldown(168, standing), 168);
    }

    #[test]
    fn one_measurement_does_not_adjust_cadence() {
        // Below minimum_measured_record (2), the worker stays untested.
        let record = AgentRecord {
            improved: 1,
            ..AgentRecord::default()
        };
        let standing = assess_agent_standing(record, policy());
        assert!(matches!(standing, AgentStanding::Untested { measured: 1 }));
        assert_eq!(effective_agent_cooldown(168, standing), 168);
    }

    #[test]
    fn effective_worker_gets_shorter_cooldown() {
        // 2 improved out of 2 measured → effectiveness = 10_000 bps → base cooldown.
        let record = AgentRecord {
            improved: 2,
            ..AgentRecord::default()
        };
        let standing = assess_agent_standing(record, policy());
        assert!(matches!(standing, AgentStanding::Weighted { .. }));
        // At max effectiveness, cooldown equals base.
        assert_eq!(effective_agent_cooldown(168, standing), 168);
    }

    #[test]
    fn ineffective_worker_gets_longer_cooldown() {
        // 0 improved, 2 neutral out of 2 → effectiveness = 5_000 bps → 2x base.
        let record = AgentRecord {
            neutral: 2,
            ..AgentRecord::default()
        };
        let standing = assess_agent_standing(record, policy());
        if let AgentStanding::Weighted {
            effectiveness_bps, ..
        } = standing
        {
            assert_eq!(effectiveness_bps, 5_000);
        } else {
            panic!("expected Weighted standing");
        }
        // 10_000 / 5_000 = 2 → 2x base cooldown.
        assert_eq!(effective_agent_cooldown(168, standing), 336);
    }

    #[test]
    fn cooldown_adjustment_is_capped_at_4x() {
        // Floor effectiveness is 2_000 bps → 10_000 / 2_000 = 5, but capped at 4x.
        let record = AgentRecord {
            worsened: 2,
            ..AgentRecord::default()
        };
        let standing = assess_agent_standing(record, policy());
        if let AgentStanding::Weighted {
            effectiveness_bps, ..
        } = standing
        {
            assert_eq!(effectiveness_bps, 2_000); // floor
        } else {
            panic!("expected Weighted standing");
        }
        // Capped at 4x base.
        assert_eq!(effective_agent_cooldown(168, standing), 168 * 4);
    }

    #[test]
    fn retired_worker_never_dispatches() {
        let record = AgentRecord {
            worsened: 3,
            consecutive_worsened: 3,
            ..AgentRecord::default()
        };
        let standing = assess_agent_standing(record, policy());
        assert!(matches!(
            standing,
            AgentStanding::Retired {
                reason: AgentRetirementReason::RepeatedlyIneffective
            }
        ));
        assert_eq!(effective_agent_cooldown(168, standing), u32::MAX);
    }

    #[test]
    fn operator_retired_worker_is_retired() {
        let record = AgentRecord {
            operator_retired: true,
            ..AgentRecord::default()
        };
        let standing = assess_agent_standing(record, policy());
        assert!(matches!(
            standing,
            AgentStanding::Retired {
                reason: AgentRetirementReason::OperatorRetired
            }
        ));
    }

    #[test]
    fn one_worsened_does_not_retire() {
        // A single bad result is noise, not a pattern.
        let record = AgentRecord {
            worsened: 1,
            consecutive_worsened: 1,
            improved: 1,
            ..AgentRecord::default()
        };
        let standing = assess_agent_standing(record, policy());
        // 2 measured, not retired.
        assert!(!standing.is_retired());
    }

    #[test]
    fn effective_worker_escalates_to_premium() {
        let record = AgentRecord {
            improved: 3,
            ..AgentRecord::default()
        };
        let standing = assess_agent_standing(record, policy());
        assert_eq!(
            effective_agent_tier(AgentTier::Basic, standing),
            AgentTier::Premium
        );
    }

    #[test]
    fn mediocre_worker_stays_at_base_tier() {
        let record = AgentRecord {
            neutral: 2,
            ..AgentRecord::default()
        };
        let standing = assess_agent_standing(record, policy());
        assert_eq!(
            effective_agent_tier(AgentTier::Basic, standing),
            AgentTier::Basic
        );
    }
}
