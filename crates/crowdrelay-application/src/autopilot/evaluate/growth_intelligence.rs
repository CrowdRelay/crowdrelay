//! Deterministic growth intelligence evaluator — the brain.
//!
//! The brain decides what intelligence to gather, when, and what to do with
//! it. LLMs are workers/tools that gather intelligence. The brain never
//! follows an LLM blindly — it applies deterministic rules and decides.
//!
//! This evaluator produces `RequestAgentRun` candidates that dispatch LLM
//! workers. Each candidate carries a deterministic prompt built from the
//! workspace's data, not from an LLM.

use crowdrelay_domain::growth_intelligence::{
    GrowthIntelligencePolicy, GrowthIntelligenceSnapshot,
};
use time::OffsetDateTime;

use super::{policy_evidence, *};

/// The deterministic decision: should the brain dispatch this worker now?
#[derive(Clone, Copy, Debug)]
pub struct IntelligenceRequest {
    pub template_id: &'static str,
    pub priority: u8,
    pub prompt: &'static str,
    pub cooldown_hours: u32,
    pub reason: &'static str,
}

/// Evaluates a snapshot deterministically. The brain applies cooldown rules
/// and situational logic — no LLM is involved in this decision.
pub fn evaluate_growth_intelligence(
    snapshot: &GrowthIntelligenceSnapshot,
    policy: &GrowthIntelligencePolicy,
) -> Option<IntelligenceRequest> {
    let hours = snapshot.hours_since_last_run.unwrap_or(u32::MAX);

    // Rule 1: Scan Reddit communities on a 7-day cadence.
    if snapshot.template_id == "reddit-scanner" && hours >= policy.reddit_scanner_cooldown_hours {
        return Some(IntelligenceRequest {
            template_id: "reddit-scanner",
            priority: 3,
            prompt: "Find Polish and Central-European metal subreddits and forums relevant to the band's genre and upcoming events. Report subscriber estimates, activity levels, and self-promo policies.",
            cooldown_hours: policy.reddit_scanner_cooldown_hours,
            reason: "Reddit community scan is due (7-day cadence)",
        });
    }

    // Rule 2: If there's an upcoming event within the lead window, pitch press.
    if snapshot.template_id == "press-pitch"
        && snapshot.has_upcoming_event
        && hours >= policy.press_pitch_cooldown_hours
    {
        let days = snapshot.days_to_next_event.unwrap_or(u32::MAX);
        if days <= policy.press_pitch_event_lead_days {
            let priority = if days <= 7 { 1 } else { 2 };
            return Some(IntelligenceRequest {
                template_id: "press-pitch",
                priority,
                prompt: "Draft press pitches for outreach targets relevant to the upcoming event. Focus on Polish metal/rock press, radio, and playlists. Reference the event details and offer an angle.",
                cooldown_hours: policy.press_pitch_cooldown_hours,
                reason: "Upcoming event within press lead window",
            });
        }
    }

    // Rule 3: Draft social content on a 2-day cadence.
    if snapshot.template_id == "social-post" && hours >= policy.social_post_cooldown_hours {
        return Some(IntelligenceRequest {
            template_id: "social-post",
            priority: 2,
            prompt: "Create social media content for the band. Reference upcoming events, recent releases, or fan milestones. Write in Polish for the primary audience. Include suggested hashtags.",
            cooldown_hours: policy.social_post_cooldown_hours,
            reason: "Social content cadence is due (2-day cycle)",
        });
    }

    // Rule 4: If fan growth is stagnant, dispatch community engagement.
    if snapshot.template_id == "community-engager"
        && snapshot.fan_growth_stagnant
        && hours >= policy.community_engager_cooldown_hours
    {
        return Some(IntelligenceRequest {
            template_id: "community-engager",
            priority: 2,
            prompt: "Draft authentic community posts for accepted outreach targets. Write like a band member, not a marketer. Match each community's tone and language. One post per community.",
            cooldown_hours: policy.community_engager_cooldown_hours,
            reason: "Fan growth stagnant — community engagement needed",
        });
    }

    // Rule 5: If there are unengaged outreach targets, draft community posts.
    if snapshot.template_id == "community-engager"
        && snapshot.unengaged_outreach_targets > 0
        && hours >= policy.community_engager_cooldown_hours
    {
        return Some(IntelligenceRequest {
            template_id: "community-engager",
            priority: 2,
            prompt: "Draft authentic community posts for the unengaged outreach targets. Write like a band member, not a marketer. Match each community's tone and language.",
            cooldown_hours: policy.community_engager_cooldown_hours,
            reason: "Unengaged outreach targets need community posts",
        });
    }

    // Rule 6: Signal inviter on a 7-day cadence.
    if snapshot.template_id == "signal-inviter" && hours >= policy.signal_inviter_cooldown_hours {
        return Some(IntelligenceRequest {
            template_id: "signal-inviter",
            priority: 3,
            prompt: "Draft Signal push invites for fans near upcoming events. Keep messages personal and under 200 characters. Include a smart link to the Signal install page. Write in Polish.",
            cooldown_hours: policy.signal_inviter_cooldown_hours,
            reason: "Signal invite cadence is due (7-day cycle)",
        });
    }

    // Rule 7: Growth strategist (intelligence analyst) on a 1-day cadence.
    if snapshot.template_id == "growth-strategist"
        && hours >= policy.growth_strategist_cooldown_hours
    {
        return Some(IntelligenceRequest {
            template_id: "growth-strategist",
            priority: 4,
            prompt: "Analyze the band's data and produce growth insights grounded in the data. Focus on opportunities and issues that affect fan aggregation, growth, or conversion.",
            cooldown_hours: policy.growth_strategist_cooldown_hours,
            reason: "Daily intelligence analysis is due",
        });
    }

    None
}

pub(super) fn growth_intelligence_candidate(
    snapshot: &GrowthIntelligenceSnapshot,
    policy: &AutopilotPolicy,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::GrowthIntelligence(domain_policy) = policy.config else {
        return Ok(None);
    };
    let Some(request) = evaluate_growth_intelligence(snapshot, &domain_policy) else {
        return Ok(None);
    };
    let disposition = disposition(
        policy.autonomy_level,
        Confidence::MAX,
        policy.minimum_confidence,
    );
    let action = AutopilotActionPayload::RequestAgentRun {
        template_id: request.template_id.to_owned(),
        prompt: request.prompt.to_owned(),
        priority: request.priority,
    };
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::Workspace(workspace_id),
        decision_kind: "request_agent_run",
        confidence: Confidence::MAX,
        disposition,
        reason: request.reason,
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy, domain_policy)?,
        action,
        decision_key: format!(
            "decision:growth-intelligence:v{}:{}:{}",
            policy.version,
            request.template_id,
            cooldown_window(now, request.cooldown_hours),
        ),
        action_idempotency_key: format!(
            "action:agent-run:{}:{}",
            request.template_id,
            cooldown_window(now, request.cooldown_hours),
        ),
    }))
}

/// Index of the cooldown window `now` falls in. Gives the action key a coarse
/// time component so the same dispatch can legitimately recur later without
/// the evaluator being able to raise it twice inside one cooldown.
fn cooldown_window(now: OffsetDateTime, cooldown_hours: u32) -> i64 {
    let window_seconds = i64::from(cooldown_hours.max(1)).saturating_mul(3_600);
    now.unix_timestamp().div_euclid(window_seconds)
}
