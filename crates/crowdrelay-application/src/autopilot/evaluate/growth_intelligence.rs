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

use crowdrelay_brain::{
    AgentTier, CausalModel, DispatchContext, DispatchPrediction, EfeWeights,
    GrowthIntelligencePolicy, GrowthIntelligenceSnapshot, GrowthOpportunity, GrowthStrategy,
    RecentInsight, effective_agent_cooldown, effective_agent_tier, information_gain,
};
use time::OffsetDateTime;

use super::{policy_evidence, *};

/// The deterministic decision: should the brain dispatch this worker now?
#[derive(Clone, Debug)]
pub struct IntelligenceRequest {
    pub template_id: &'static str,
    pub priority: u8,
    pub prompt: String,
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
    /// Treatment-aware stats from the causal model. Used by the portfolio
    /// optimizer to rank candidates by Y30 durable fans (North Star) when
    /// the treatment-effect confidence is sufficient.
    pub treatment_stats: crowdrelay_brain::TreatmentAwareStats,
}

/// Formats the unengaged outreach targets into a context block for the
/// community-engager dispatch prompt. The LLM needs the concrete
/// `target_id` and `subreddit` to produce `social_post` outcomes that
/// result in `community.engage.request` actions — without this list the
/// LLM can only produce generic content, which falls through to the
/// `agent.content.request` path and never reaches Reddit.
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

/// Builds the enriched dispatch context from a snapshot and the current
/// time. This is shared between the novelty lookup in `evaluate.rs` and
/// the prediction in `evaluate_growth_intelligence` so both use the same
/// context hash — otherwise the novelty score would be computed against
/// a different key than what gets recorded.
pub(super) fn build_dispatch_context(
    snapshot: &GrowthIntelligenceSnapshot,
    now: OffsetDateTime,
) -> DispatchContext {
    let subreddit_type = snapshot
        .unengaged_targets
        .first()
        .map(|t| classify_subreddit(&t.subreddit))
        .or_else(|| {
            snapshot
                .community_engagement_history
                .first()
                .map(|c| classify_subreddit(&c.subreddit))
        });
    let post_format = template_post_format(&snapshot.template_id);
    let time_of_day_bps = time_of_day_to_bps(now.hour());
    let community_novelty_bps = community_novelty_bps(&snapshot.community_engagement_history);
    DispatchContext {
        days_to_event: snapshot.days_to_next_event,
        fan_growth_trend: snapshot.world_model.fan_growth_trend,
        subreddit_type,
        post_format,
        time_of_day_bps,
        community_novelty_bps,
    }
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
/// - **Epistemic value**: information gain × prediction uncertainty
///   (`predict_std / sqrt(1 + confidence)` — variance-aware, not just
///   count-aware).
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
    strategy_posterior: &crowdrelay_brain::StateConditionedStrategyPosterior,
    now: OffsetDateTime,
) -> Option<IntelligenceRequest> {
    // A retired worker is never dispatched. The standing is computed from
    // past measurement outcomes — a worker that consistently produces no fan
    // growth or worsens it is retired until an operator reinstates it.
    if snapshot.standing.is_retired() {
        return None;
    }

    // Build the enriched dispatch context (shared with the novelty lookup
    // in evaluate.rs so both use the same context hash).
    let dispatch_context = build_dispatch_context(snapshot, now);
    // Treatment-aware EFE scoring: when the treatment-effect model has enough
    // paired experiment data (≥ MIN_TREATMENT_CONFIDENCE), the brain uses τ
    // (the causally correct treatment effect) as the primary ranking signal
    // instead of the outcome model P(Y|action,context). Below that threshold,
    // it falls back to the outcome model.
    let treatment_stats =
        causal_model.predict_stats_with_treatment(&snapshot.template_id, &dispatch_context);
    let (expected_new_fans, predict_std, confidence) = if treatment_stats.use_treatment_effect {
        (
            treatment_stats.treatment_effect,
            treatment_stats.treatment_std,
            treatment_stats.treatment_confidence,
        )
    } else {
        (
            treatment_stats.expected_fans,
            treatment_stats.predict_std,
            treatment_stats.confidence,
        )
    };
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
    let info_gain = information_gain(confidence, predict_std);
    let efe_weights = EfeWeights::default();

    // Strategy rank — used for candidate ELIGIBILITY and sort ordering,
    // NOT for modifying expected fan value. The rank is the position of
    // this template in the strategy's recommended priority list.
    // `usize::MAX` means the template is not in the strategy's list.
    let strategy_rank = strategy
        .template_priority()
        .iter()
        .position(|t| *t == snapshot.template_id)
        .unwrap_or(usize::MAX);

    // ── Strategy as gate, not score (P0 — EFE cleanup) ──
    // Previously the strategy posterior multiplied expected fan value via
    // hand-tuned coefficients (0.7×, 0.8×, 0.9×, 1.1×). This was a second
    // prediction model sitting on top of the actual causal model, injecting
    // arbitrary bias into the fan-value estimate. It let a bad action with
    // a good strategy rank look artificially great.
    //
    // The strategy posterior is now kept for candidate ELIGIBILITY and
    // EXPLORATION ALLOCATION — it influences which templates are considered
    // and where to explore — but it NEVER alters the predicted fan value.
    // The causal model is the sole authority for expected fans.
    //
    // Strategy is a SOFT BIAS, not a hard filter: a strategy preference
    // must never silently eliminate an action with substantially higher
    // expected incremental Y30 unless an explicit policy requires it.
    //
    // Long-term: strategy should enter the predictive model directly as
    // E[Y30 | action, audience, context, strategy] — learned as a feature,
    // not hacked in after prediction.
    //
    // NOTE: `strategy` is used above for `strategy_rank` (eligibility/sort).
    // `strategy_posterior` is used for exploration allocation in the caller.
    let _ = &strategy_posterior; // used for exploration allocation, not scoring

    // ── Time-to-feedback discount (P1.10) ──
    // Templates that produce feedback faster are slightly preferred because
    // the brain can learn from them sooner. The discount is based on the
    // actual feedback horizon — the time until the measurement window
    // closes and the brain observes the outcome.
    //
    // The feedback horizon is determined by the measurement kind:
    //   - Y14 (incremental fan growth): 14 days
    //   - Y30 (durable fan growth): 44 days (14-day window + 30-day check)
    //   - Signal installs: 7 days
    //   - Scanner/strategist proximal outcomes: 14 days
    //
    // The discount is multiplicative on the EFE score (lower = better):
    //   short horizon (≤7 days) → 0.92× (8% boost — learn fastest)
    //   medium horizon (≤14 days) → 0.95× (5% boost)
    //   long horizon (≤44 days) → 0.98× (2% boost)
    //   very long horizon (>44 days) → 1.0× (no boost)
    //
    // Previously this used confidence buckets as a proxy for feedback speed,
    // which is wrong: confidence measures how much we've learned, not how
    // fast we learn. A template with 100 observations still has a 14-day
    // feedback horizon.
    let feedback_horizon_days = feedback_horizon_for_template(&snapshot.template_id);
    let feedback_discount = if feedback_horizon_days <= 7 {
        0.92 // 8% boost for fast feedback
    } else if feedback_horizon_days <= 14 {
        0.95 // 5% boost
    } else if feedback_horizon_days <= 44 {
        0.98 // 2% boost
    } else {
        1.0 // no boost for very long horizons
    };

    let raw_efe = GrowthOpportunity::compute_efe(
        expected_new_fans,
        info_gain,
        predict_std,
        exploration_novelty,
        efe_weights,
    );
    let efe_score = raw_efe * feedback_discount;

    // Adaptive cadence: the effective cooldown is adjusted by the worker's
    // measured standing. Effective workers get shorter cooldowns (dispatched
    // more often), ineffective ones get longer cooldowns (dispatched less).
    let reddit_scanner_cd =
        effective_agent_cooldown(policy.reddit_scanner_cooldown_hours, snapshot.standing);
    let press_pitch_cd =
        effective_agent_cooldown(policy.press_pitch_cooldown_hours, snapshot.standing);
    let social_post_cd =
        effective_agent_cooldown(policy.social_post_cooldown_hours, snapshot.standing);
    // community-engager cooldown is used by community_engager_candidates,
    // not by this function. This function handles direct-action templates
    // only (social-post, signal-inviter, growth-strategist, booking-finder).
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
            treatment_stats,
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
                treatment_stats,
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
            treatment_stats,
        });
    }

    // community-engager is handled by community_engager_candidates in
    // growth_intelligence_candidate, which produces one candidate per
    // target community. This function is never called with
    // template_id == "community-engager" — the rules below are for the
    // remaining direct-action templates only.

    // Rule 6: Signal inviter on a 2-day cadence, escalated near events.
    // When an event is within 14 days, the cadence tightens to 1 day and
    // priority rises — fans need time to plan attendance, and a push sent
    // the day before is too late.
    if snapshot.template_id == "signal-inviter"
        && effective_hours >= signal_inviter_cd
        && retry_ready
    {
        let days_to_event = snapshot.days_to_next_event.unwrap_or(u32::MAX);
        let (priority, reason) = if days_to_event <= 14 {
            (1, "Signal invite escalated — event within 14 days")
        } else {
            (3, "Signal invite cadence is due (2-day cycle)")
        };
        let mut prompt = "Draft Signal push invites for fans near upcoming events. Keep messages personal and under 200 characters. Include a smart link to the Signal install page. Write in Polish.".to_owned();
        if !insights.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&insights);
        }
        return Some(IntelligenceRequest {
            template_id: "signal-inviter",
            priority,
            prompt,
            key_window_hours: if is_retry {
                retry_window
            } else {
                signal_inviter_cd
            },
            reason,
            tier: effective_agent_tier(AgentTier::Basic, snapshot.standing),
            prediction: make_prediction(
                "signal-inviter",
                expected_new_fans,
                expected_signal_installs,
                &dispatch_context,
            ),
            efe_score,
            strategy_rank,
            treatment_stats,
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
            treatment_stats,
        });
    }

    None
}

/// A scored growth intelligence candidate: (decision, prediction, EFE score,
/// strategy rank, treatment-aware stats). Used by the portfolio optimizer.
pub(super) type ScoredCandidate = (
    DecisionCandidate,
    DispatchPrediction,
    f64,
    usize,
    crowdrelay_brain::TreatmentAwareStats,
);

#[allow(clippy::too_many_arguments)]
pub(super) fn growth_intelligence_candidate(
    snapshot: &GrowthIntelligenceSnapshot,
    policy: &AutopilotPolicy,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
    causal_model: &CausalModel,
    strategy: GrowthStrategy,
    exploration_novelty: f64,
    strategy_posterior: &crowdrelay_brain::StateConditionedStrategyPosterior,
) -> Result<Vec<ScoredCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::GrowthIntelligence(ref domain_policy) = policy.config else {
        return Ok(Vec::new());
    };
    // community-engager produces one candidate per target community.
    // Each community is a distinct experimental unit (TargetCommunity),
    // with its own decision_key, idempotency key, and prediction context.
    // This enables per-community randomized holdout: "does engaging r/djent
    // produce incremental durable fans versus not engaging r/djent?"
    if snapshot.template_id == "community-engager" {
        return community_engager_candidates(
            snapshot,
            policy,
            domain_policy,
            workspace_id,
            now,
            causal_model,
            strategy,
            exploration_novelty,
            strategy_posterior,
        );
    }
    // All other templates: 0 or 1 workspace-wide candidate.
    let Some(request) = evaluate_growth_intelligence(
        snapshot,
        domain_policy,
        causal_model,
        strategy,
        exploration_novelty,
        strategy_posterior,
        now,
    ) else {
        return Ok(Vec::new());
    };
    Ok(vec![candidate_from_request(
        &request,
        snapshot,
        policy,
        domain_policy,
        workspace_id,
        now,
    )?])
}

/// Builds a `ScoredCandidate` from an `IntelligenceRequest` for non-community
/// templates. The decision_key and idempotency key are workspace-wide
/// (template + cooldown window).
fn candidate_from_request(
    request: &IntelligenceRequest,
    snapshot: &GrowthIntelligenceSnapshot,
    policy: &AutopilotPolicy,
    domain_policy: &GrowthIntelligencePolicy,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<ScoredCandidate, serde_json::Error> {
    let prediction = request.prediction.clone();
    let efe_score = request.efe_score;
    let strategy_rank = request.strategy_rank;
    let treatment_stats = request.treatment_stats;
    let disposition = disposition(
        policy.autonomy_level,
        Confidence::MAX,
        policy.minimum_confidence,
    );
    let action = AutopilotActionPayload::RequestAgentRun {
        template_id: request.template_id.to_owned(),
        prompt: request.prompt.clone(),
        priority: request.priority,
        tier: request.tier,
    };
    Ok((
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
        treatment_stats,
    ))
}

/// Produces one candidate per unengaged target community for the
/// community-engager template.
///
/// P0-3: community-engager is intrinsically a community-scoped intervention.
/// Each target community is a distinct experimental unit
/// (`ExperimentUnitKind::TargetCommunity`). The decision_key includes the
/// target_id so each community has its own idempotency, cooldown, and
/// experiment assignment. This enables per-community randomized holdout:
/// "does engaging r/djent produce incremental durable fans versus not
/// engaging r/djent?"
///
/// One candidate per community means the portfolio optimizer can choose
/// between actual opportunities (r/djent → +4.2, r/metalcore → +0.8)
/// rather than treating the entire engager template as one workspace action.
#[allow(clippy::too_many_arguments)]
fn community_engager_candidates(
    snapshot: &GrowthIntelligenceSnapshot,
    policy: &AutopilotPolicy,
    domain_policy: &GrowthIntelligencePolicy,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
    causal_model: &CausalModel,
    strategy: GrowthStrategy,
    exploration_novelty: f64,
    _strategy_posterior: &crowdrelay_brain::StateConditionedStrategyPosterior,
) -> Result<Vec<ScoredCandidate>, serde_json::Error> {
    // Check cooldown — if the template is not due, no candidates.
    let community_engager_cd = effective_agent_cooldown(
        domain_policy.community_engager_cooldown_hours,
        snapshot.standing,
    );
    let effective_hours = snapshot.hours_since_last_effective_run.unwrap_or(u32::MAX);
    let any_hours = snapshot.hours_since_last_run.unwrap_or(u32::MAX);
    let retry_ready = any_hours >= domain_policy.failed_run_retry_hours;
    if effective_hours < community_engager_cd || !retry_ready {
        return Ok(Vec::new());
    }
    // If there are no unengaged targets, no candidates.
    if snapshot.unengaged_targets.is_empty() {
        return Ok(Vec::new());
    }
    let disposition = disposition(
        policy.autonomy_level,
        Confidence::MAX,
        policy.minimum_confidence,
    );
    let is_retry = snapshot.hours_since_last_effective_run.is_none();
    let retry_window = domain_policy.failed_run_retry_hours.max(1);
    let key_window_hours = if is_retry {
        retry_window
    } else {
        community_engager_cd
    };
    let insights = insights_block(&snapshot.recent_insights);
    let strategy_rank = strategy
        .template_priority()
        .iter()
        .position(|t| *t == "community-engager")
        .unwrap_or(usize::MAX);
    let mut candidates = Vec::with_capacity(snapshot.unengaged_targets.len());
    for target in &snapshot.unengaged_targets {
        // Build a per-community dispatch context. The subreddit_type is
        // the specific community's classification, not the first target's.
        let subreddit_type = classify_subreddit(&target.subreddit);
        let post_format = template_post_format("community-engager");
        let time_of_day_bps = time_of_day_to_bps(now.hour());
        // Per-community novelty: use this community's engagement history.
        let community_history: Vec<_> = snapshot
            .community_engagement_history
            .iter()
            .filter(|h| h.subreddit.eq_ignore_ascii_case(&target.subreddit))
            .cloned()
            .collect();
        let community_novelty_bps = community_novelty_bps(&community_history);
        let dispatch_context = DispatchContext {
            days_to_event: snapshot.days_to_next_event,
            fan_growth_trend: snapshot.world_model.fan_growth_trend,
            subreddit_type: Some(subreddit_type.clone()),
            post_format: post_format.clone(),
            time_of_day_bps,
            community_novelty_bps,
        };
        // Treatment-aware stats for this specific community context.
        let treatment_stats =
            causal_model.predict_stats_with_treatment("community-engager", &dispatch_context);
        let (expected_new_fans, predict_std, confidence) = if treatment_stats.use_treatment_effect {
            (
                treatment_stats.treatment_effect,
                treatment_stats.treatment_std,
                treatment_stats.treatment_confidence,
            )
        } else {
            (
                treatment_stats.expected_fans,
                treatment_stats.predict_std,
                treatment_stats.confidence,
            )
        };
        let expected_signal_installs =
            causal_model.predict_signal("community-engager", &dispatch_context);
        let info_gain = information_gain(confidence, predict_std);
        let efe_weights = EfeWeights::default();
        let raw_efe = GrowthOpportunity::compute_efe(
            expected_new_fans,
            info_gain,
            predict_std,
            exploration_novelty,
            efe_weights,
        );
        // 14-day feedback horizon for community-engager (Y14).
        let efe_score = raw_efe * 0.95;
        // Per-community prompt: one post for this specific community.
        let mut prompt = format!(
            "Draft an authentic community post for r/{}. Write like a band member, not a marketer. Match this community's tone and language.",
            target.subreddit
        );
        prompt.push_str(&format!(
            "\n\n- target_id: {}, subreddit: {} ({})",
            target.target_id, target.subreddit, target.display_name
        ));
        if !community_history.is_empty() {
            push_engagement_history(&mut prompt, &community_history);
        }
        if !insights.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&insights);
        }
        let prediction = DispatchPrediction {
            template_id: "community-engager".to_owned(),
            expected_new_fans,
            expected_signal_installs,
            context: dispatch_context,
        };
        let action = AutopilotActionPayload::RequestAgentRun {
            template_id: "community-engager".to_owned(),
            prompt,
            priority: 2,
            tier: effective_agent_tier(AgentTier::Premium, snapshot.standing),
        };
        // Per-community decision_key: includes target_id so each community
        // has its own idempotency, cooldown, and experiment assignment.
        let community_unit_id = format!("r/{}", target.subreddit);
        candidates.push((
            DecisionCandidate {
                context: policy.context,
                subject: ActionSubject::Workspace(workspace_id),
                decision_kind: "request_agent_run",
                confidence: Confidence::MAX,
                disposition,
                reason: if snapshot.fan_growth_stagnant {
                    "Fan growth stagnant — community engagement needed"
                } else {
                    "Unengaged outreach target needs a community post"
                },
                input_snapshot: serde_json::json!({
                    "snapshot": snapshot,
                    "prediction": &prediction,
                    "target_id": target.target_id,
                    "subreddit": target.subreddit,
                }),
                policy_snapshot: policy_evidence(policy, domain_policy)?,
                action,
                decision_key: format!(
                    "decision:growth-intelligence:v{}:community-engager:{}:{}",
                    policy.version,
                    target.target_id,
                    cooldown_window(now, key_window_hours),
                ),
                action_idempotency_key: format!(
                    "action:agent-run:community-engager:{}:{}",
                    target.target_id,
                    cooldown_window(now, key_window_hours),
                ),
            },
            prediction,
            efe_score,
            strategy_rank,
            treatment_stats,
        ));
        // The community_unit_id is used later as the experiment unit_id.
        // We store it in the decision_key's structure — the caller extracts
        // it from the candidate's decision_key or constructs it from the
        // target. For now, the unit_id is derived in the context arm.
        let _ = &community_unit_id;
    }
    Ok(candidates)
}

/// Index of the cooldown window `now` falls in. Gives the action key a coarse
/// time component so the same dispatch can legitimately recur later without
/// the evaluator being able to raise it twice inside one cooldown.
pub(super) fn cooldown_window(now: OffsetDateTime, cooldown_hours: u32) -> i64 {
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
    history: &[crowdrelay_brain::CommunityEngagementSummary],
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

// ── Context enrichment helpers (Phase 1.5) ──

/// Classifies a subreddit name into a genre type for context-level learning.
/// The causal model uses this to pool observations across subreddits of the
/// same genre — "r/MetalMusic" and "r/heavymetal" both map to "metal".
pub(super) fn classify_subreddit(subreddit: &str) -> String {
    let lower = subreddit.to_lowercase();
    if lower.contains("metal") {
        "metal".to_owned()
    } else if lower.contains("prog") {
        "prog".to_owned()
    } else if lower.contains("polish") || lower.contains("polska") || lower.contains("pl") {
        "polish".to_owned()
    } else if lower.contains("rock") {
        "rock".to_owned()
    } else if lower.contains("jazz") {
        "jazz".to_owned()
    } else if lower.contains("indie") {
        "indie".to_owned()
    } else if lower.contains("electronic") || lower.contains("edm") {
        "electronic".to_owned()
    } else if lower.contains("hiphop") || lower.contains("rap") {
        "hiphop".to_owned()
    } else {
        "other".to_owned()
    }
}

/// Returns the feedback horizon (in days) for a worker template — the time
/// until the brain observes the outcome of a dispatch. This is determined by
/// the measurement kind scheduled for the template:
///   - Scanner/strategist: 14 days (proximal outcome measurement)
///   - Signal inviter: 7 days (signal install measurement)
///   - Direct-action workers: 14 days (Y14) + 44 days (Y30)
///
/// The shortest horizon is used for the feedback discount — the brain learns
/// the fastest signal first.
fn feedback_horizon_for_template(template_id: &str) -> u32 {
    match template_id {
        "signal-inviter" => 7,
        // Scanner and strategist have proximal outcome measurements (14d).
        "reddit-scanner" | "growth-strategist" => 14,
        // Direct-action workers have Y14 (14d) as the shortest horizon.
        _ => 14,
    }
}

/// Returns the default post format for a worker template. Different templates
/// produce different content formats — reddit-scanner produces text reports,
/// social-post produces social media posts, etc.
fn template_post_format(template_id: &str) -> Option<String> {
    match template_id {
        "reddit-scanner" => Some("text_report".to_owned()),
        "community-engager" => Some("text_post".to_owned()),
        "social-post" => Some("social_post".to_owned()),
        "press-pitch" => Some("email_pitch".to_owned()),
        "signal-inviter" => Some("direct_message".to_owned()),
        "growth-strategist" => Some("text_report".to_owned()),
        _ => None,
    }
}

/// Converts the hour-of-day (0–23) to basis points (0–10_000).
/// 0 = midnight, 4167 = 10am, 6250 = 3pm, 8333 = 8pm.
fn time_of_day_to_bps(hour: u8) -> u16 {
    ((hour as u32 * 10_000) / 24).min(10_000) as u16
}

/// Computes community novelty in basis points (0–10_000).
/// 10_000 = completely novel (no engagement history), 0 = well-explored.
fn community_novelty_bps(history: &[crowdrelay_brain::CommunityEngagementSummary]) -> u16 {
    if history.is_empty() {
        return 10_000;
    }
    // More history = less novel. Cap at 10_000.
    let count = history.len() as u32;
    let novelty = 10_000_u32.saturating_sub(count.saturating_mul(1_000));
    novelty as u16
}
