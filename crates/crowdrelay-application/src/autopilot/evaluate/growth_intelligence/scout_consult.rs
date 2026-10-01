//! Dispatch for the two deep-research intelligence templates — split out
//! of `growth_intelligence.rs` so the evaluator stays under the
//! source-size ratchet while the prompts keep room to say what they are
//! for.
//!
//! Both are weekly, both are premium tier with a free-chain fallback
//! inside the runner, and both exist to answer questions the deterministic
//! brain cannot ask itself:
//!
//! - `fanbase-scout` — "where are my future fans?" It searches the web
//!   and the Reddit scrape cache for communities across every platform,
//!   grounded by real URLs rather than model memory. Premium because the
//!   answer is only as good as the reasoning over messy search results.
//! - `strategy-consult` — the consultant audits what the system is doing
//!   against what it is getting and proposes typed improvements; the
//!   deterministic evaluator implements or rejects each proposal with a
//!   reason the consultant reads back next run. Premium for the same
//!   reason — advice grounded in weak reasoning produces verdict noise
//!   the brain then has to spend cycles rejecting.

use super::*;
use crowdrelay_domain::worker_template::{TemplateAudience, WorkerTemplate};

/// Per-template dispatch constants.
struct DispatchSpec {
    /// The static template id — `IntelligenceRequest.template_id` is a
    /// `&'static str`, so the spec carries the literal rather than the
    /// snapshot's owned string.
    template_id: &'static str,
    /// The base question the worker answers. Fanbase discovery extends this
    /// with a deterministic Brain-authored brief from live world state.
    prompt: &'static str,
    /// Priority within the cycle.
    priority: u8,
    /// Which policy cooldown field governs the cadence.
    cooldown_field: fn(&GrowthIntelligencePolicy) -> u32,
    /// The due-reason recorded on the request.
    reason: &'static str,
}

fn fanbase_research_brief(snapshot: &GrowthIntelligenceSnapshot) -> String {
    let mut brief = String::from(
        "You are a research worker delegated by CrowdRelay Brain. This run is discovery only:          find new public fan-bearing places and return evidence-backed targets for screening.          Do not contact people, do not post, and do not infer permission to promote. Search beyond          predefined forums: Reddit, Discord, Telegram, Facebook groups, independent forums, local          scene/community sites and other public places where the act's plausible fans already gather.          Prefer specific, live places over generic directories. Every proposed target needs a real URL          found in this run, and duplicates already known to the workspace are not useful."
    );

    if let Some(days) = snapshot.days_to_next_event
        && days <= 30
    {
        brief.push_str(&format!(
            "\n\nEVENT-LOCAL MODE: the nearest published show is in {days} day(s).              The worker context includes that event's city, venue and date. Use them. Search the city              and surrounding scene for communities and public gathering places that can plausibly              affect attendance. Also look at adjacent local music/culture scenes rather than only              genre-labelled forums. This is research for relevance, not permission to promote."
        ));
    }

    if let Some(objective) = &snapshot.world_model.objective {
        brief.push_str(&format!(
            "\n\nDECLARED OBJECTIVE CONTEXT: {}:{} is currently {:?}; observed {:?}, target {} by {}.              Use this to understand what growth the operator cares about, but never turn deadline              pressure into lower relevance standards or more aggressive outreach.",
            objective.platform,
            objective.metric_key,
            objective.state,
            objective.observed_value,
            objective.target_value,
            objective.deadline
        ));
    }

    if snapshot.fan_growth_stagnant {
        brief.push_str(
            "\n\nFan growth is currently stagnant. Increase breadth of research, not outreach pressure:              explore genuinely new scenes, regions and community types, while keeping the same evidence              and fit bar."
        );
    }

    brief
}

fn dispatch_spec(template_id: &str) -> Option<DispatchSpec> {
    match template_id {
        "fanbase-scout" => Some(DispatchSpec {
            template_id: "fanbase-scout",
            prompt: "Where are my future fans? Actively search the open web for new, evidence-backed places where people who could genuinely care about this act already gather. Do not limit yourself to the communities already in our database, and never invent a place or URL from memory.",
            priority: 3,
            cooldown_field: |policy| policy.fanbase_scout_cooldown_hours,
            reason: "Weekly fanbase discovery scan is due",
        }),
        "strategy-consult" => Some(DispatchSpec {
            template_id: "strategy-consult",
            prompt: "Consult my strategy on getting more fans and suggest real improvements that may work. Propose only typed actions — communities to join, scans to rerun, cadences to adjust, queries to add, things for the operator — each with the evidence behind it.",
            priority: 4,
            cooldown_field: |policy| policy.strategy_consult_cooldown_hours,
            reason: "Weekly strategy consultation is due",
        }),
        _ => None,
    }
}

/// Whether a pending rescan request may pull this snapshot's template
/// forward once. An accepted `rescan` proposal lands a request row the
/// loader sets here; the dispatch it enables consumes the row in the same
/// transaction, so the bypass cannot loop. It applies only to
/// intelligence templates: a consult asking a posting worker to run
/// early is out of scope for the closed action list, and the evaluator
/// enforces that even when the verdict-side filter missed.
pub(super) fn rescan_bypass_active(snapshot: &GrowthIntelligenceSnapshot) -> bool {
    snapshot.rescan_requested
        && WorkerTemplate::parse(&snapshot.template_id)
            .is_some_and(|t| t.audience() == TemplateAudience::Intelligence)
}

/// Rules 8 and 9 of `evaluate_growth_intelligence`: dispatch the fanbase
/// scout or the strategy consultant when the snapshot names the template
/// and its cooldown is satisfied. Returns `None` for any other template
/// or when the gate fails — the caller's fallback then applies.
#[allow(clippy::too_many_arguments)]
pub(super) fn scout_consult_dispatch(
    snapshot: &GrowthIntelligenceSnapshot,
    policy: &GrowthIntelligencePolicy,
    effective_hours: u32,
    retry_ready: bool,
    is_retry: bool,
    retry_window: u32,
    insights: &str,
    expected_new_fans: f64,
    expected_signal_installs: f64,
    dispatch_context: &DispatchContext,
    treatment_stats: crowdrelay_brain::TreatmentAwareStats,
    efe_score: f64,
    strategy_rank: usize,
    info_gain: f64,
    exploration_novelty: f64,
) -> Option<IntelligenceRequest> {
    let spec = dispatch_spec(&snapshot.template_id)?;

    // Same standing + tenant-preference adjustment every rule applies —
    // `apply_pref` in the evaluator, spelled out the way
    // `community_engager_candidates` does.
    let base = effective_agent_cooldown((spec.cooldown_field)(policy), snapshot.standing);
    let pref_mult = snapshot
        .tenant_preference
        .cadence_multiplier(&snapshot.template_id);
    let discovery_cap_mult = policy.tenant_preference_policy.discovery_cadence_cap;
    let mut cooldown = ((f64::from(base) * pref_mult).round() as u32)
        .max(1)
        .min(((f64::from(base) * discovery_cap_mult).round() as u32).max(1))
        .max(1);
    // A nearby show makes fresh local intelligence perishable. Research is
    // read-only and still competes in the normal portfolio, so shortening the
    // scout cadence is not permission to post more or contact more people.
    if spec.template_id == "fanbase-scout"
        && snapshot.days_to_next_event.is_some_and(|days| days <= 21)
    {
        cooldown = cooldown.min(72);
    }
    if effective_hours < cooldown || !retry_ready {
        return None;
    }

    let mut prompt = if spec.template_id == "fanbase-scout" {
        format!("{}\n\n{}", spec.prompt, fanbase_research_brief(snapshot))
    } else {
        spec.prompt.to_owned()
    };
    if !insights.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(insights);
    }
    Some(IntelligenceRequest {
        template_id: spec.template_id,
        priority: spec.priority,
        prompt,
        key_window_hours: if is_retry { retry_window } else { cooldown },
        reason: spec.reason,
        tier: effective_agent_tier(AgentTier::Premium, snapshot.standing),
        prediction: make_prediction(
            spec.template_id,
            expected_new_fans,
            expected_signal_installs,
            dispatch_context,
            &treatment_stats.secondary,
        ),
        efe_score,
        strategy_rank,
        treatment_stats,
        information_gain: info_gain,
        novelty: exploration_novelty,
    })
}

#[cfg(test)]
mod dispatch_rule_tests {
    use super::*;
    use crowdrelay_brain::{
        AgentExecutionHealth, TenantPreferencePosterior, WorldModel, hypothesis::HypothesisState,
        self_assessment::MetacognitionMonitor,
    };
    use crowdrelay_domain::learning::Standing;

    /// A dispatch-ready snapshot for one template: hypothesis active,
    /// standing untested, nothing pending. Tests mutate the two clock
    /// fields and `rescan_requested`.
    fn snapshot_for(template_id: &str) -> GrowthIntelligenceSnapshot {
        GrowthIntelligenceSnapshot {
            fatigue: None,
            template_id: template_id.to_owned(),
            hours_since_last_run: Some(500),
            hours_since_last_effective_run: Some(500),
            has_upcoming_event: false,
            days_to_next_event: None,
            fan_growth_stagnant: false,
            unengaged_outreach_targets: 0,
            unengaged_targets: Vec::new(),
            recent_insights: Vec::new(),
            community_engagement_history: Vec::new(),
            social_content_history: Vec::new(),
            standing: Standing::Untested { measured: 0 },
            world_model: WorldModel::default(),
            tenant_preference: TenantPreferencePosterior::default(),
            hypothesis_state: HypothesisState::Active,
            metacognition: MetacognitionMonitor::default(),
            agent_execution_health: AgentExecutionHealth::default(),
            rescan_requested: false,
        }
    }

    fn evaluate(snapshot: &GrowthIntelligenceSnapshot) -> Option<IntelligenceRequest> {
        evaluate_growth_intelligence(
            snapshot,
            &GrowthIntelligencePolicy::default(),
            &CausalModel::default(),
            GrowthStrategy::AggressiveDiscovery,
            0.0,
            OffsetDateTime::now_utc(),
        )
    }

    #[test]
    fn fanbase_scout_dispatches_weekly_at_premium_tier() {
        let request = evaluate(&snapshot_for("fanbase-scout"))
            .expect("a 500h-old scout snapshot is past its 168h cooldown");
        assert_eq!(request.template_id, "fanbase-scout");
        assert_eq!(request.tier, AgentTier::Premium);
        assert!(request.prompt.contains("Where are my future fans?"));
    }

    #[test]
    fn fanbase_scout_waits_inside_its_cooldown() {
        let mut snapshot = snapshot_for("fanbase-scout");
        snapshot.hours_since_last_run = Some(50);
        snapshot.hours_since_last_effective_run = Some(50);
        assert!(evaluate(&snapshot).is_none());
    }

    #[test]
    fn nearby_event_turns_fanbase_scout_into_local_open_web_research() {
        let mut snapshot = snapshot_for("fanbase-scout");
        snapshot.days_to_next_event = Some(16);
        snapshot.has_upcoming_event = true;
        snapshot.hours_since_last_run = Some(80);
        snapshot.hours_since_last_effective_run = Some(80);

        let request = evaluate(&snapshot)
            .expect("read-only local research refreshes inside the ordinary weekly cadence");
        assert!(request.prompt.contains("EVENT-LOCAL MODE"));
        assert!(request.prompt.contains("Search beyond predefined forums"));
        assert!(request.prompt.contains("Do not contact people"));
    }

    #[test]
    fn fanbase_scout_receives_declared_objective_without_turning_it_into_spam_pressure() {
        use crowdrelay_brain::goal::ActiveObjective;
        use crowdrelay_domain::{
            growth_metrics::MetricDirection,
            objectives::ObjectiveState,
        };

        let mut snapshot = snapshot_for("fanbase-scout");
        snapshot.world_model.objective = Some(ActiveObjective {
            objective_id: uuid::Uuid::from_u128(7),
            platform: "signal".to_owned(),
            metric_key: "active_fans".to_owned(),
            direction: MetricDirection::HigherIsBetter,
            baseline_value: 3,
            target_value: 100,
            observed_value: Some(3),
            declared_at: OffsetDateTime::now_utc() - time::Duration::days(1),
            deadline: OffsetDateTime::now_utc() + time::Duration::days(30),
            state: ObjectiveState::Behind {
                progress_basis_points: 0,
                projected_value: 5,
                shortfall: 95,
            },
        });

        let request = evaluate(&snapshot).expect("scout dispatch");
        assert!(request.prompt.contains("signal:active_fans"));
        assert!(request.prompt.contains("target 100"));
        assert!(request.prompt.contains("never turn deadline pressure into lower relevance standards"));
    }

    #[test]
    fn strategy_consult_dispatches_weekly_at_premium_tier() {
        let request = evaluate(&snapshot_for("strategy-consult"))
            .expect("a 500h-old consult snapshot is past its 168h cooldown");
        assert_eq!(request.template_id, "strategy-consult");
        assert_eq!(request.tier, AgentTier::Premium);
        assert!(request.prompt.contains("Consult my strategy"));
    }

    #[test]
    fn a_rescan_pulls_an_intelligence_template_forward() {
        let mut snapshot = snapshot_for("reddit-scanner");
        // Inside its 72h cooldown — without the rescan this does not fire.
        snapshot.hours_since_last_run = Some(10);
        snapshot.hours_since_last_effective_run = Some(10);
        assert!(evaluate(&snapshot).is_none(), "no rescan, no dispatch");

        snapshot.rescan_requested = true;
        let request = evaluate(&snapshot).expect("a pending rescan bypasses cooldown");
        assert_eq!(request.template_id, "reddit-scanner");
    }

    #[test]
    fn a_rescan_cannot_pull_a_posting_template_forward() {
        let mut snapshot = snapshot_for("social-post");
        snapshot.hours_since_last_run = Some(10);
        snapshot.hours_since_last_effective_run = Some(10);
        snapshot.rescan_requested = true;
        // SocialPost is Workspace-audience — a strategy proposal asking for
        // it early is out of the closed vocabulary's reach, and the
        // evaluator enforces that even when the verdict-side check missed.
        assert!(evaluate(&snapshot).is_none());
    }
}
