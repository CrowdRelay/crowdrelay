//! Snapshot types — the data the brain receives each cycle.
//!
//! These are the snapshot types loaded by the infra layer and consumed by
//! the application-layer evaluator. They carry everything the brain needs
//! to decide what to dispatch.

use crowdrelay_domain::learning::Standing;
use serde::{Deserialize, Serialize};

use crate::world_model::WorldModel;

/// A recent unconsumed insight from an agent outcome.
#[derive(Clone, Debug, Serialize)]
pub struct RecentInsight {
    pub outcome_id: uuid::Uuid,
    pub template_id: String,
    pub kind: String,
    pub headline: String,
    pub detail: String,
    pub recommended_action: Option<String>,
}

/// A snapshot of one worker template's dispatch state.
#[derive(Clone, Debug, Serialize)]
pub struct GrowthIntelligenceSnapshot {
    pub template_id: String,
    pub hours_since_last_run: Option<u32>,
    pub hours_since_last_effective_run: Option<u32>,
    pub has_upcoming_event: bool,
    pub days_to_next_event: Option<u32>,
    pub fan_growth_stagnant: bool,
    pub unengaged_outreach_targets: u32,
    pub unengaged_targets: Vec<UnengagedTarget>,
    pub recent_insights: Vec<RecentInsight>,
    pub community_engagement_history: Vec<CommunityEngagementSummary>,
    pub standing: Standing,
    pub world_model: WorldModel,
}

/// A single unengaged outreach target.
#[derive(Clone, Debug, Serialize)]
pub struct UnengagedTarget {
    pub target_id: uuid::Uuid,
    pub display_name: String,
    pub subreddit: String,
}

/// Aggregated performance of a single subreddit's recent community posts.
#[derive(Clone, Debug, Serialize)]
pub struct CommunityEngagementSummary {
    pub subreddit: String,
    pub post_count: u32,
    pub avg_score: f64,
    pub avg_upvotes: f64,
    pub avg_comments: f64,
    pub avg_upvote_ratio: Option<f64>,
}

/// Cooldown intervals (in hours) for each worker template.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct GrowthIntelligencePolicy {
    pub reddit_scanner_cooldown_hours: u32,
    pub community_engager_cooldown_hours: u32,
    pub press_pitch_cooldown_hours: u32,
    pub social_post_cooldown_hours: u32,
    pub signal_inviter_cooldown_hours: u32,
    pub growth_strategist_cooldown_hours: u32,
    pub press_pitch_event_lead_days: u32,
    pub fan_growth_stagnant_days: u32,
    pub failed_run_retry_hours: u32,
    /// Probability (0.0–1.0) that a high-volume dispatch is randomly
    /// assigned to a holdout control group. The control group is not
    /// dispatched but is still measured — the difference between the
    /// treatment and control groups is the true causal treatment effect.
    ///
    /// Guardrails:
    /// - 0.0 = no holdout (all dispatches go through, DiD counterfactual only)
    /// - 0.05 = 5% of dispatches are held out (recommended for high-volume)
    /// - Maximum 0.10 (10%) — higher values would waste too many opportunities
    ///
    /// The holdout only applies to direct-action workers (community-engager,
    /// social-post, signal-inviter). Scanner and strategist are never held
    /// out — they don't directly acquire fans, so there's no treatment to
    /// withhold.
    pub randomized_holdout_probability: f64,
}

impl Default for GrowthIntelligencePolicy {
    fn default() -> Self {
        Self {
            reddit_scanner_cooldown_hours: 168,
            community_engager_cooldown_hours: 120,
            press_pitch_cooldown_hours: 72,
            social_post_cooldown_hours: 48,
            signal_inviter_cooldown_hours: 48,
            growth_strategist_cooldown_hours: 24,
            press_pitch_event_lead_days: 30,
            fan_growth_stagnant_days: 14,
            failed_run_retry_hours: 1,
            randomized_holdout_probability: 0.0,
        }
    }
}
