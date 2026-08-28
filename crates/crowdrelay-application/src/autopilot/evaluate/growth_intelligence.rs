//! Deterministic growth intelligence evaluator — the brain.
//!
//! The brain decides what intelligence to gather, when, and what to do with
//! it. LLMs are workers/tools that gather intelligence. The brain never
//! follows an LLM blindly — it applies deterministic rules and decides.
//!
//! This evaluator produces `RequestAgentRun` candidates that dispatch LLM
//! workers. Each candidate carries a deterministic prompt built from the
//! workspace's data, not from an LLM.
//!
//! # Scoring: Expected Free Energy (EFE)
//!
//! Each eligible dispatch is scored by EFE, an Active Inference metric that
//! balances pragmatic value (expected fan growth) against epistemic value
//! (information gain from reducing uncertainty). Lower EFE = better
//! opportunity. The brain dispatches the lowest-EFE opportunities first,
//! so when budget limits kick in the best opportunities are already
//! enqueued.
//!
//! # Strategy: hierarchical planning
//!
//! The brain derives a `GrowthStrategy` from the world model each cycle.
//! The strategy determines template priority order — which workers the
//! brain dispatches first when multiple are eligible. This is the
//! hierarchical layer: strategy → template priority → EFE tie-break.

use crowdrelay_domain::growth_intelligence::{
    AgentTier, CausalModel, DispatchContext, DispatchPrediction, EfeWeights,
    GrowthIntelligencePolicy, GrowthIntelligenceSnapshot, GrowthOpportunity, GrowthStrategy,
    RecentInsight, UnengagedTarget, effective_agent_cooldown, effective_agent_tier,
    information_gain,
};
use time::OffsetDateTime;

use super::{policy_evidence, *};

/// The deterministic decision: should the brain dispatch this worker now?
#[derive(Clone, Debug)]
pub struct IntelligenceRequest {
    pub template_id: &'static str,
    pub priority: u8,
    pub prompt: String,
    #[allow(dead_code)]
    pub cooldown_hours: u32,
    /// The time window used for the decision/action idempotency key. For a
    /// normal dispatch (after a successful run), this equals `cooldown_hours`.
    /// For a retry after a failed/empty run, this equals the retry delay
    /// (1 hour) so the key changes each hour and allows retry.
    pub key_window_hours: u32,
    pub reason: &'static str,
    /// Intelligent token optimization: basic tasks go to free models,
    /// premium tasks go to connected paid providers. Defaults to basic.
    pub tier: AgentTier,
    /// The brain's prediction for this dispatch: how many new fans and
    /// Signal installs it expects. Recorded before dispatch so the
    /// prediction error can be computed after measurement.
    pub prediction: DispatchPrediction,
    /// Expected Free Energy score: lower = better opportunity.
    /// Combines expected fan growth (pragmatic) with information gain
    /// (epistemic). The brain dispatches lowest-EFE opportunities first.
    pub efe_score: f64,
    /// The strategy-derived priority rank for this template (0 = highest
    /// priority). Used as the primary sort key before EFE.
    pub strategy_rank: usize,
}

/// Formats the unengaged outreach targets into a context block for the
/// community-engager dispatch prompt. The LLM needs the concrete
/// `target_id` and `subreddit` to produce `social_post` outcomes that
/// result in `community.engage.request` actions — without this list the
/// LLM can only produce generic content, which falls through to the
/// `agent.content.request` path and never reaches Reddit.
fn unengaged_targets_block(targets: &[UnengagedTarget]) -> String {
    if targets.is_empty() {
        return String::new();
    }
    let mut lines = Vec::with_capacity(targets.len() + 2);
    lines.push(
        "Draft one post per target. Each post MUST include the target_id \
         and subreddit from the list below in the item fields."
            .to_owned(),
    );
    for target in targets {
        lines.push(format!(
            "- target_id: {}, subreddit: {} ({})",
            target.target_id, target.subreddit, target.display_name
        ));
    }
    lines.join("\n")
}

/// Formats recent insights into a context block for the dispatch prompt.
/// This closes the feedback loop: the worker sees what previous runs already
/// discovered and can focus on new ground instead of repeating itself.
fn insights_block(insights: &[RecentInsight]) -> String {
    if insights.is_empty() {
        return String::new();
    }
    let mut lines = Vec::with_capacity(insights.len() + 1);
    lines.push("Previous insights from your last run (do NOT repeat these — build on them or find new angles):".to_owned());
    for insight in insights {
        let action = insight
            .recommended_action
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("");
        let action_suffix = if action.is_empty() {
            String::new()
        } else {
            format!(" → recommended: {action}")
        };
        lines.push(format!(
            "- [{}] {} — {}{}",
            insight.kind, insight.headline, insight.detail, action_suffix
        ));
    }
    lines.join("\n")
}

/// Evaluates a snapshot deterministically. The brain applies cooldown rules
/// and situational logic — no LLM is involved in this decision. Recent
/// insights from previous worker runs are included in the dispatch prompt
/// so the worker can build on them rather than repeating itself.
///
/// # EFE scoring
///
/// Each eligible dispatch gets an EFE score that combines:
/// - **Pragmatic value**: expected fan growth from the causal model.
/// - **Epistemic value**: information gain from reducing uncertainty
///   (1/(1+confidence) — unmeasured templates have maximum information gain).
/// - **Exploration bonus**: novelty from the exploration memory, so the
///   brain prefers unexplored (template, context) combinations.
///
/// Lower EFE = better opportunity. The caller sorts by EFE before
/// persisting, so budget limits hit the worst opportunities first.
pub fn evaluate_growth_intelligence(
    snapshot: &GrowthIntelligenceSnapshot,
    policy: &GrowthIntelligencePolicy,
    causal_model: &CausalModel,
    strategy: GrowthStrategy,
    exploration_novelty: f64,
) -> Option<IntelligenceRequest> {
    // A retired worker is never dispatched. The standing is computed from
    // past measurement outcomes — a worker that consistently produces no fan
    // growth or worsens it is retired until an operator reinstates it.
    if snapshot.standing.is_retired() {
        return None;
    }

    // Build the dispatch context from the world model for prediction.
    let dispatch_context = DispatchContext {
        days_to_event: snapshot.days_to_next_event,
        fan_growth_trend: snapshot.world_model.fan_growth_trend,
        subreddit_type: None,
        post_format: None,
        time_of_day_bps: 0,
        community_novelty_bps: 0,
    };
    // Predict expected new fans using the causal model.
    let expected_new_fans = causal_model.predict(&snapshot.template_id, &dispatch_context);
    // Predict expected Signal installs using the learned Signal model.
    let expected_signal_installs =
        causal_model.predict_signal(&snapshot.template_id, &dispatch_context);

    // ── EFE scoring with uncertainty ──
    // The full EFE formula:
    //   EFE = -(w_prag * expected_fans
    //         + w_epist * info_gain * predict_std
    //         + w_explore * novelty)
    //         + w_risk * predict_std
    //
    // - Pragmatic: expected fan growth (exploitation).
    // - Epistemic: information gain × prediction uncertainty (exploration
    //   drive — the brain dispatches workers it's uncertain about).
    // - Exploration: novelty from the exploration memory (Go-Explore bonus).
    // - Risk: penalizes uncertain outcomes (risk aversion).
    let confidence = causal_model.confidence(&snapshot.template_id);
    let info_gain = information_gain(confidence);
    let predict_std = causal_model.predict_std(&snapshot.template_id);
    let efe_weights = EfeWeights::default();
    let efe_score = GrowthOpportunity::compute_efe(
        expected_new_fans,
        info_gain,
        predict_std,
        exploration_novelty,
        efe_weights,
    );

    // ── Strategy rank ──
    // The strategy determines which templates the brain prioritizes.
    // Rank 0 = highest priority. Templates not in the strategy list get
    // a rank of usize::MAX so they sort last.
    let strategy_priority = strategy.template_priority();
    let strategy_rank = strategy_priority
        .iter()
        .position(|t| *t == snapshot.template_id)
        .unwrap_or(usize::MAX);

    // Adaptive cadence: the effective cooldown is adjusted by the worker's
    // measured standing. Effective workers get shorter cooldowns (dispatched
    // more often), ineffective ones get longer cooldowns (dispatched less).
    let reddit_scanner_cd =
        effective_agent_cooldown(policy.reddit_scanner_cooldown_hours, snapshot.standing);
    let press_pitch_cd =
        effective_agent_cooldown(policy.press_pitch_cooldown_hours, snapshot.standing);
    let social_post_cd =
        effective_agent_cooldown(policy.social_post_cooldown_hours, snapshot.standing);
    let community_engager_cd =
        effective_agent_cooldown(policy.community_engager_cooldown_hours, snapshot.standing);
    let signal_inviter_cd =
        effective_agent_cooldown(policy.signal_inviter_cooldown_hours, snapshot.standing);
    let growth_strategist_cd =
        effective_agent_cooldown(policy.growth_strategist_cooldown_hours, snapshot.standing);

    // Layered cooldown: the effective cooldown only counts runs that produced
    // items (outreach targets, social posts, etc.). A failed/empty run does
    // NOT reset the cooldown, but the retry delay prevents 5-minute retry
    // storms on the autopilot cycle.
    let effective_hours = snapshot.hours_since_last_effective_run.unwrap_or(u32::MAX);
    let any_hours = snapshot.hours_since_last_run.unwrap_or(u32::MAX);
    let retry_ready = any_hours >= policy.failed_run_retry_hours;
    let insights = insights_block(&snapshot.recent_insights);

    // When the last run was not effective (no items produced), use the retry
    // delay as the idempotency key window so the key changes each hour and
    // allows retry. When the last run was effective, use the full cooldown.
    let is_retry = snapshot.hours_since_last_effective_run.is_none();
    let retry_window = policy.failed_run_retry_hours.max(1);

    /// Builds a DispatchPrediction for the given template.
    fn make_prediction(
        template_id: &str,
        expected_new_fans: f64,
        expected_signal_installs: f64,
        context: &DispatchContext,
    ) -> DispatchPrediction {
        DispatchPrediction {
            template_id: template_id.to_owned(),
            expected_new_fans,
            expected_signal_installs,
            context: context.clone(),
        }
    }

    // Rule 1: Scan Reddit communities on a 7-day cadence.
    if snapshot.template_id == "reddit-scanner"
        && effective_hours >= reddit_scanner_cd
        && retry_ready
    {
        let mut prompt = "Find Polish and Central-European metal subreddits and forums relevant to the band's genre and upcoming events. Report subscriber estimates, activity levels, and self-promo policies.".to_owned();
        if !insights.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&insights);
        }
        return Some(IntelligenceRequest {
            template_id: "reddit-scanner",
            priority: 3,
            prompt,
            cooldown_hours: reddit_scanner_cd,
            key_window_hours: if is_retry {
                retry_window
            } else {
                reddit_scanner_cd
            },
            reason: "Reddit community scan is due (7-day cadence)",
            tier: effective_agent_tier(AgentTier::Basic, snapshot.standing),
            prediction: make_prediction(
                "reddit-scanner",
                expected_new_fans,
                expected_signal_installs,
                &dispatch_context,
            ),
            efe_score,
            strategy_rank,
        });
    }

    // Rule 2: If there's an upcoming event within the lead window, pitch press.
    if snapshot.template_id == "press-pitch"
        && snapshot.has_upcoming_event
        && effective_hours >= press_pitch_cd
        && retry_ready
    {
        let days = snapshot.days_to_next_event.unwrap_or(u32::MAX);
        if days <= policy.press_pitch_event_lead_days {
            let priority = if days <= 7 { 1 } else { 2 };
            let mut prompt = "Draft press pitches for outreach targets relevant to the upcoming event. Focus on Polish metal/rock press, radio, and playlists. Reference the event details and offer an angle.".to_owned();
            if !insights.is_empty() {
                prompt.push_str("\n\n");
                prompt.push_str(&insights);
            }
            return Some(IntelligenceRequest {
                template_id: "press-pitch",
                priority,
                prompt,
                cooldown_hours: press_pitch_cd,
                key_window_hours: if is_retry {
                    retry_window
                } else {
                    press_pitch_cd
                },
                reason: "Upcoming event within press lead window",
                // Premium: press pitches go to real human contacts. A bad
                // pitch burns a relationship permanently — quality matters.
                // The standing may escalate or maintain this; a worker with
                // poor standing stays at base tier even for press pitches.
                tier: effective_agent_tier(AgentTier::Premium, snapshot.standing),
                prediction: make_prediction(
                    "press-pitch",
                    expected_new_fans,
                    expected_signal_installs,
                    &dispatch_context,
                ),
                efe_score,
                strategy_rank,
            });
        }
    }

    // Rule 3: Draft social content on a 2-day cadence.
    if snapshot.template_id == "social-post" && effective_hours >= social_post_cd && retry_ready {
        let mut prompt = "Create social media content for the band. Reference upcoming events, recent releases, or fan milestones. Write in Polish for the primary audience. Include suggested hashtags.".to_owned();
        if !insights.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&insights);
        }
        return Some(IntelligenceRequest {
            template_id: "social-post",
            priority: 2,
            prompt,
            cooldown_hours: social_post_cd,
            key_window_hours: if is_retry {
                retry_window
            } else {
                social_post_cd
            },
            reason: "Social content cadence is due (2-day cycle)",
            tier: effective_agent_tier(AgentTier::Basic, snapshot.standing),
            prediction: make_prediction(
                "social-post",
                expected_new_fans,
                expected_signal_installs,
                &dispatch_context,
            ),
            efe_score,
            strategy_rank,
        });
    }

    // Rule 4: If fan growth is stagnant, dispatch community engagement.
    if snapshot.template_id == "community-engager"
        && snapshot.fan_growth_stagnant
        && effective_hours >= community_engager_cd
        && retry_ready
    {
        let mut prompt = "Draft authentic community posts for accepted outreach targets. Write like a band member, not a marketer. Match each community's tone and language. One post per community.".to_owned();
        let targets_block = unengaged_targets_block(&snapshot.unengaged_targets);
        if !targets_block.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&targets_block);
        }
        push_engagement_history(&mut prompt, &snapshot.community_engagement_history);
        if !insights.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&insights);
        }
        return Some(IntelligenceRequest {
            template_id: "community-engager",
            priority: 2,
            prompt,
            cooldown_hours: community_engager_cd,
            key_window_hours: if is_retry {
                retry_window
            } else {
                community_engager_cd
            },
            reason: "Fan growth stagnant — community engagement needed",
            // Premium: posts to somebody else's community (Reddit). A bad
            // post gets banned and damages reputation — quality matters.
            tier: effective_agent_tier(AgentTier::Premium, snapshot.standing),
            prediction: make_prediction(
                "community-engager",
                expected_new_fans,
                expected_signal_installs,
                &dispatch_context,
            ),
            efe_score,
            strategy_rank,
        });
    }

    // Rule 5: If there are unengaged outreach targets, draft community posts.
    if snapshot.template_id == "community-engager"
        && snapshot.unengaged_outreach_targets > 0
        && effective_hours >= community_engager_cd
        && retry_ready
    {
        let mut prompt = "Draft authentic community posts for the unengaged outreach targets. Write like a band member, not a marketer. Match each community's tone and language.".to_owned();
        let targets_block = unengaged_targets_block(&snapshot.unengaged_targets);
        if !targets_block.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&targets_block);
        }
        push_engagement_history(&mut prompt, &snapshot.community_engagement_history);
        if !insights.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&insights);
        }
        return Some(IntelligenceRequest {
            template_id: "community-engager",
            priority: 2,
            prompt,
            cooldown_hours: community_engager_cd,
            key_window_hours: if is_retry {
                retry_window
            } else {
                community_engager_cd
            },
            reason: "Unengaged outreach targets need community posts",
            tier: effective_agent_tier(AgentTier::Premium, snapshot.standing),
            prediction: make_prediction(
                "community-engager",
                expected_new_fans,
                expected_signal_installs,
                &dispatch_context,
            ),
            efe_score,
            strategy_rank,
        });
    }

    // Rule 6: Signal inviter on a 7-day cadence.
    if snapshot.template_id == "signal-inviter"
        && effective_hours >= signal_inviter_cd
        && retry_ready
    {
        let mut prompt = "Draft Signal push invites for fans near upcoming events. Keep messages personal and under 200 characters. Include a smart link to the Signal install page. Write in Polish.".to_owned();
        if !insights.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&insights);
        }
        return Some(IntelligenceRequest {
            template_id: "signal-inviter",
            priority: 3,
            prompt,
            cooldown_hours: signal_inviter_cd,
            key_window_hours: if is_retry {
                retry_window
            } else {
                signal_inviter_cd
            },
            reason: "Signal invite cadence is due (7-day cycle)",
            tier: effective_agent_tier(AgentTier::Basic, snapshot.standing),
            prediction: make_prediction(
                "signal-inviter",
                expected_new_fans,
                expected_signal_installs,
                &dispatch_context,
            ),
            efe_score,
            strategy_rank,
        });
    }

    // Rule 7: Growth strategist (intelligence analyst) on a 1-day cadence.
    // This is the primary insight producer — previous insights are especially
    // important here so the strategist doesn't re-discover the same findings.
    // Situational escalation: if fan growth has been stagnant for an extended
    // period (>= 2x the stagnation threshold), escalate to premium for deeper
    // analysis — the situation is serious and warrants a more powerful model.
    if snapshot.template_id == "growth-strategist"
        && effective_hours >= growth_strategist_cd
        && retry_ready
    {
        let stagnant_escalation = snapshot.fan_growth_stagnant;
        let base_tier = if stagnant_escalation {
            AgentTier::Premium
        } else {
            AgentTier::Basic
        };
        let mut prompt = "Analyze the band's data and produce growth insights grounded in the data. Focus on opportunities and issues that affect fan aggregation, growth, or conversion.".to_owned();
        if stagnant_escalation {
            prompt.push_str("\n\nWARNING: Fan growth has been stagnant. This is a serious situation — provide deeper, more actionable analysis than usual.");
        }
        if !insights.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&insights);
        }
        return Some(IntelligenceRequest {
            template_id: "growth-strategist",
            priority: 4,
            prompt,
            cooldown_hours: growth_strategist_cd,
            key_window_hours: if is_retry {
                retry_window
            } else {
                growth_strategist_cd
            },
            reason: if stagnant_escalation {
                "Daily intelligence analysis is due (escalated to premium: stagnant growth)"
            } else {
                "Daily intelligence analysis is due"
            },
            tier: effective_agent_tier(base_tier, snapshot.standing),
            prediction: make_prediction(
                "growth-strategist",
                expected_new_fans,
                expected_signal_installs,
                &dispatch_context,
            ),
            efe_score,
            strategy_rank,
        });
    }

    None
}

pub(super) fn growth_intelligence_candidate(
    snapshot: &GrowthIntelligenceSnapshot,
    policy: &AutopilotPolicy,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
    causal_model: &CausalModel,
    strategy: GrowthStrategy,
    exploration_novelty: f64,
) -> Result<Option<(DecisionCandidate, DispatchPrediction, f64, usize)>, serde_json::Error> {
    let AutopilotPolicyConfig::GrowthIntelligence(domain_policy) = policy.config else {
        return Ok(None);
    };
    let Some(request) = evaluate_growth_intelligence(
        snapshot,
        &domain_policy,
        causal_model,
        strategy,
        exploration_novelty,
    ) else {
        return Ok(None);
    };
    let prediction = request.prediction.clone();
    let efe_score = request.efe_score;
    let strategy_rank = request.strategy_rank;
    let disposition = disposition(
        policy.autonomy_level,
        Confidence::MAX,
        policy.minimum_confidence,
    );
    let action = AutopilotActionPayload::RequestAgentRun {
        template_id: request.template_id.to_owned(),
        prompt: request.prompt,
        priority: request.priority,
        tier: request.tier,
    };
    Ok(Some((
        DecisionCandidate {
            context: policy.context,
            subject: ActionSubject::Workspace(workspace_id),
            decision_kind: "request_agent_run",
            confidence: Confidence::MAX,
            disposition,
            reason: request.reason,
            input_snapshot: serde_json::json!({
                "snapshot": snapshot,
                "prediction": &request.prediction,
            }),
            policy_snapshot: policy_evidence(policy, domain_policy)?,
            action,
            decision_key: format!(
                "decision:growth-intelligence:v{}:{}:{}",
                policy.version,
                request.template_id,
                cooldown_window(now, request.key_window_hours),
            ),
            action_idempotency_key: format!(
                "action:agent-run:{}:{}",
                request.template_id,
                cooldown_window(now, request.key_window_hours),
            ),
        },
        prediction,
        efe_score,
        strategy_rank,
    )))
}

/// Index of the cooldown window `now` falls in. Gives the action key a coarse
/// time component so the same dispatch can legitimately recur later without
/// the evaluator being able to raise it twice inside one cooldown.
fn cooldown_window(now: OffsetDateTime, cooldown_hours: u32) -> i64 {
    let window_seconds = i64::from(cooldown_hours.max(1)).saturating_mul(3_600);
    now.unix_timestamp().div_euclid(window_seconds)
}

/// Appends community engagement performance history to the prompt so the
/// LLM worker knows which subreddits respond well and which don't. This is
/// the feedback loop: the brain feeds post performance data forward to the
/// worker so it can write better posts and avoid wasting effort on dead
/// communities.
fn push_engagement_history(
    prompt: &mut String,
    history: &[crowdrelay_domain::growth_intelligence::CommunityEngagementSummary],
) {
    if history.is_empty() {
        return;
    }
    prompt.push_str("\n\n## Community Post Performance History\n");
    prompt.push_str("Recent post performance by subreddit (use this to guide your approach):\n");
    for entry in history {
        let ratio = entry
            .avg_upvote_ratio
            .map(|r| format!("{:.0}%", r * 100.0))
            .unwrap_or_else(|| "n/a".to_owned());
        prompt.push_str(&format!(
            "- r/{}: {} posts, avg {} upvotes, avg {} comments, {} upvote ratio, avg score {}\n",
            entry.subreddit,
            entry.post_count,
            entry.avg_upvotes.round() as i64,
            entry.avg_comments.round() as i64,
            ratio,
            entry.avg_score.round() as i64,
        ));
    }
    prompt.push_str(
        "Communities with near-zero engagement may not be worth posting to again. \
         Communities with good engagement are worth nurturing — match what worked.",
    );
}
