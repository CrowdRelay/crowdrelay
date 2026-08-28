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
    pub hours_since_last_run: Option<u32>,
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
        }
    }
}
