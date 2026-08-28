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
