//! Snapshot types — the data the brain receives each cycle.
//!
//! These are the snapshot types loaded by the infra layer and consumed by
//! the application-layer evaluator. They carry everything the brain needs
//! to decide what to dispatch.

use std::collections::HashMap;

use crowdrelay_domain::learning::Standing;
use serde::{Deserialize, Serialize};

use crate::tenant_preference::{TenantPreferencePolicy, TenantPreferencePosterior};
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
    /// Tenant operating preference posterior — how this tenant tends to
    /// accept/reject each template. Influences candidate surfacing,
    /// ordering, and cadence. MUST NOT modify DecisionValue or any
    /// economic value.
    pub tenant_preference: TenantPreferencePosterior,
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
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
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
    /// P0-2: Minimum experiment capacity — the minimum number of eligible
    /// units required to start a randomized holdout experiment. Below this,
    /// candidates execute observationally — no holdout, no fake causal
    /// claim. This is a capacity guard, NOT a formal statistical power
    /// calculation. Default 10.
    #[serde(default = "default_min_eligible_units_for_experiment")]
    pub min_eligible_units_for_experiment: u32,
    /// P0-2: Additional treatment dispatch slots for active experiments.
    /// These slots allow the brain to deliberately buy
    /// information-producing treatment opportunities beyond the normal
    /// `max_dispatches` budget. The normal action budget remains the
    /// primary operational budget; this is a learning budget. Slots are
    /// consumed ONLY by treatment assignments in active experiments
    /// (control assignments consume zero slots). The budget is NOT spent
    /// blindly — experimental candidates must still clear safety/value
    /// gates. Default 3.
    #[serde(default = "default_experimental_dispatch_budget")]
    pub experimental_dispatch_budget: u32,
    /// Minimum expected control arm size. A 10% holdout on 3 units gives
    /// 0 controls — this guard prevents that. Default 2.
    #[serde(default = "default_min_expected_control_units")]
    pub min_expected_control_units: u32,
    /// Minimum expected treatment arm size. Default 2.
    #[serde(default = "default_min_expected_treatment_units")]
    pub min_expected_treatment_units: u32,
    /// Per-template operator-configured resource costs. These are NOT
    /// measured costs — they are tunable knobs for the portfolio optimizer.
    /// The architecture can learn/calibrate them later.
    /// Keys are template IDs (e.g. "reddit-scanner", "community-engager").
    /// Missing keys default to 1.0.
    #[serde(default = "default_template_costs")]
    pub template_costs: HashMap<String, f64>,
    /// Tenant operating preference policy — controls how the brain learns
    /// and applies per-tenant template preferences. Influences candidate
    /// surfacing, ordering, and cadence. MUST NOT modify DecisionValue.
    #[serde(default)]
    pub tenant_preference_policy: TenantPreferencePolicy,
}

fn default_min_eligible_units_for_experiment() -> u32 {
    10
}

fn default_experimental_dispatch_budget() -> u32 {
    3
}

fn default_min_expected_control_units() -> u32 {
    2
}

fn default_min_expected_treatment_units() -> u32 {
    2
}

fn default_template_costs() -> HashMap<String, f64> {
    let mut m = HashMap::new();
    m.insert("reddit-scanner".to_string(), 0.5);
    m.insert("community-engager".to_string(), 2.0);
    m.insert("press-pitch".to_string(), 3.0);
    m.insert("signal-inviter".to_string(), 1.5);
    m.insert("social-post".to_string(), 1.5);
    m.insert("growth-strategist".to_string(), 4.0);
    m
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
            min_eligible_units_for_experiment: default_min_eligible_units_for_experiment(),
            experimental_dispatch_budget: default_experimental_dispatch_budget(),
            min_expected_control_units: default_min_expected_control_units(),
            min_expected_treatment_units: default_min_expected_treatment_units(),
            template_costs: default_template_costs(),
            tenant_preference_policy: TenantPreferencePolicy::default(),
        }
    }
}

impl GrowthIntelligencePolicy {
    /// Returns the operator-configured resource cost for a template.
    /// Falls back to 1.0 if the template is not in the cost map.
    #[must_use]
    pub fn template_cost(&self, template_id: &str) -> f64 {
        self.template_costs.get(template_id).copied().unwrap_or(1.0)
    }
}
