//! Candidate generation for the community engager — one candidate per
//! community the brain may post to.
//!
//! Split out of `growth_intelligence.rs` so the per-community reasoning has
//! room to be explicit. This is where target quality actually enters the
//! decision: the community's measured size, its own promotion rules, how
//! long since we last posted there, and the per-target level of the causal
//! model.

use super::*;
use crowdrelay_domain::creative::CreativeFamily;

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
pub(super) fn community_engager_candidates(
    snapshot: &GrowthIntelligenceSnapshot,
    policy: &AutopilotPolicy,
    domain_policy: &GrowthIntelligencePolicy,
    _workspace_id: WorkspaceId,
    now: OffsetDateTime,
    causal_model: &CausalModel,
    strategy: GrowthStrategy,
    exploration_novelty: f64,
    _strategy_posterior: &crowdrelay_brain::StateConditionedStrategyPosterior,
) -> Result<Vec<ScoredCandidate>, serde_json::Error> {
    // Check cooldown — if the template is not due, no candidates.
    // Apply tenant preference cadence multiplier (see
    // evaluate_growth_intelligence for details).
    let pref_mult = snapshot
        .tenant_preference
        .cadence_multiplier(&snapshot.template_id);
    let discovery_cap_mult = domain_policy.tenant_preference_policy.discovery_cadence_cap;
    let community_engager_cd = {
        let base = effective_agent_cooldown(
            domain_policy.community_engager_cooldown_hours,
            snapshot.standing,
        );
        let pref_adjusted = ((f64::from(base) * pref_mult).round() as u32).max(1);
        let discovery_cap = ((f64::from(base) * discovery_cap_mult).round() as u32).max(1);
        pref_adjusted.min(discovery_cap).max(1)
    };
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
        .template_priority_for(&snapshot.world_model)
        .iter()
        .position(|t| *t == "community-engager")
        .unwrap_or(usize::MAX);
    let mut candidates = Vec::with_capacity(snapshot.unengaged_targets.len());
    for target in &snapshot.unengaged_targets {
        // A community that asks for a longer gap between promotional posts
        // than we have left it gets no candidate at all. This is the
        // community's own rule, so it is a gate rather than a penalty the
        // portfolio could outbid. The default matches
        // `discovery_place_rules.cooldown_days`.
        if !target.cooldown_elapsed(DEFAULT_COMMUNITY_COOLDOWN_DAYS) {
            continue;
        }
        // Build a per-community dispatch context. The subreddit_type is
        // the specific community's classification, not the first target's.
        let subreddit_type = classify_community(target);
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
        // Treatment-aware stats for this specific community. The target key
        // is what makes two communities in the same genre bucket predict
        // differently — without it every candidate in this loop carried an
        // identical value and the portfolio was choosing at random among
        // them.
        let target_key = target.target_key();
        let treatment_stats = causal_model.predict_stats_with_treatment_for_target(
            "community-engager",
            Some(&target_key),
            &dispatch_context,
        );
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
        // Novelty is per community, not per cycle. The caller's scalar is
        // the template-level novelty; a community the brain has never posted
        // to is more novel than one it posts to weekly, and collapsing the
        // two made exploration unable to tell targets apart.
        let exploration_novelty = target_novelty(exploration_novelty, target);
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
        // The angle this post takes, rotated per community so every family
        // gets comparable exposure in each one. Recorded on the prediction
        // and the evidence row; nothing ranks by it yet.
        let posts_so_far = community_history.iter().map(|h| h.post_count).sum::<u32>();
        let creative_family =
            CreativeFamily::rotate_with_event(posts_so_far, snapshot.has_upcoming_event);
        // Per-community prompt: one post for this specific community.
        let mut prompt = format!(
            "Draft an authentic community post for r/{}. Write like a band member, not a marketer. Match this community's tone and language.",
            target.subreddit
        );
        prompt.push_str(&format!(
            "\n\n- target_id: {}, subreddit: {} ({})",
            target.target_id, target.subreddit, target.display_name
        ));
        if let Some(members) = target.member_count {
            prompt.push_str(&format!("\n- members: {members}"));
        }
        // The community's own promotion rule is the difference between a
        // post that stays up and one that gets the band banned, so the
        // worker is told it rather than left to guess from tone.
        if let Some(ratio) = target.self_promo_ratio_percent {
            prompt.push_str(&format!(
                "\n- this community allows at most {ratio}% self-promotional content: lead with something worth reading on its own"
            ));
        }
        prompt.push_str("\n\n");
        prompt.push_str(creative_family.brief());
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
            target_key: Some(target_key.clone()),
            creative_family: Some(creative_family),
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
        candidates.push(ScoredCandidate {
            candidate: DecisionCandidate {
                context: policy.context,
                subject: ActionSubject::TargetCommunity(target.target_id),
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
            information_gain: info_gain,
            novelty: exploration_novelty,
        });
        // The community_unit_id is used later as the experiment unit_id.
        // We store it in the decision_key's structure — the caller extracts
        // it from the candidate's decision_key or constructs it from the
        // target. For now, the unit_id is derived in the context arm.
        let _ = &community_unit_id;
    }
    Ok(candidates)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crowdrelay_brain::UnengagedTarget;

    fn target(subreddit: &str) -> UnengagedTarget {
        UnengagedTarget {
            target_id: uuid::Uuid::nil(),
            display_name: subreddit.to_owned(),
            subreddit: subreddit.to_owned(),
            member_count: Some(20_000),
            activity_basis_points: Some(4_000),
            genres: Vec::new(),
            self_promo_ratio_percent: Some(10),
            cooldown_days: Some(14),
            days_since_last_engagement: None,
        }
    }

    #[test]
    fn recorded_genre_beats_a_guess_from_the_name() {
        // "r/PostRockPlaylists" is not a Polish community, and the substring
        // fallback used to say it was. A recorded genre is evidence.
        let mut t = target("PostRockPlaylists");
        t.genres = vec!["post-rock".to_owned()];
        assert_eq!(classify_community(&t), "post-rock");
    }

    #[test]
    fn name_classification_is_the_fallback_when_no_genre_was_recorded() {
        assert_eq!(classify_community(&target("MetalMusic")), "metal");
    }

    #[test]
    fn playlist_names_are_no_longer_classified_as_polish() {
        // The old cascade tested `contains("pl")` before `rock`, so every
        // subreddit with "playlist" in its name landed in the Polish bucket.
        assert_eq!(classify_subreddit("MusicPlaylists"), "other");
        assert_eq!(classify_subreddit("ProgPlaylist"), "prog");
        // Actual Polish communities still classify correctly.
        assert_eq!(classify_subreddit("polska"), "polish");
        assert_eq!(classify_subreddit("PolishRock"), "polish");
    }

    #[test]
    fn a_community_inside_its_own_cooldown_is_not_a_candidate() {
        let mut t = target("djent");
        t.cooldown_days = Some(21);
        t.days_since_last_engagement = Some(10);
        assert!(!t.cooldown_elapsed(DEFAULT_COMMUNITY_COOLDOWN_DAYS));
        t.days_since_last_engagement = Some(21);
        assert!(t.cooldown_elapsed(DEFAULT_COMMUNITY_COOLDOWN_DAYS));
    }

    #[test]
    fn a_community_never_posted_to_is_always_eligible() {
        let t = target("djent");
        assert!(t.cooldown_elapsed(DEFAULT_COMMUNITY_COOLDOWN_DAYS));
    }

    #[test]
    fn an_unstated_cooldown_falls_back_to_the_default() {
        let mut t = target("djent");
        t.cooldown_days = None;
        t.days_since_last_engagement = Some(13);
        assert!(!t.cooldown_elapsed(DEFAULT_COMMUNITY_COOLDOWN_DAYS));
        t.days_since_last_engagement = Some(14);
        assert!(t.cooldown_elapsed(DEFAULT_COMMUNITY_COOLDOWN_DAYS));
    }

    #[test]
    fn novelty_recovers_with_the_gap_since_the_last_post() {
        let mut t = target("djent");
        assert!((target_novelty(1.0, &t) - 1.0).abs() < 1e-12);
        t.days_since_last_engagement = Some(0);
        assert!((target_novelty(1.0, &t) - 0.0).abs() < 1e-12);
        t.days_since_last_engagement = Some(7);
        assert!((target_novelty(1.0, &t) - 0.5).abs() < 1e-12);
        // Novelty never exceeds the template-level score it starts from.
        t.days_since_last_engagement = Some(400);
        assert!((target_novelty(1.0, &t) - 1.0).abs() < 1e-12);
    }
}
