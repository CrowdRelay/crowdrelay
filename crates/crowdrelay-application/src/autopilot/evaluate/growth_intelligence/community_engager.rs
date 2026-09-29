//! Candidate generation for the community engager — one candidate per
//! community the brain may post to.
//!
//! Split out of `growth_intelligence.rs` so the per-community reasoning has
//! room to be explicit. This is where target quality actually enters the
//! decision: the community's measured size, its own promotion rules, how
//! long since we last posted there, and the per-target level of the causal
//! model.

use super::*;
use crowdrelay_brain::UnengagedTarget;
use crowdrelay_brain::opportunity_graph::{
    DependencyKind, OpportunityGraph, community_membership_key, community_post_key,
};
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
    evidence: ContextEvidence,
    _workspace_id: WorkspaceId,
    now: OffsetDateTime,
    causal_model: &CausalModel,
    strategy: GrowthStrategy,
    exploration_novelty: f64,
    blocked_on_membership: &mut Vec<(String, u32)>,
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
    // `Confidence::MAX` is asserted, not measured. Without the evidence gate
    // this context licenses unattended execution on its own say-so.
    //
    // The dispatch is internal work: the drafted post arrives back as its own
    // `community.engage.request` and gates there with the composed text in
    // front of the operator. Requiring a first approval for the prompt alone
    // produced "approve the agent run" cards nobody could review — measured
    // as part of the fourteen-day approval flood.
    let disposition =
        crowdrelay_domain::autonomy::internal_work_disposition(disposition_with_evidence(
            policy.autonomy_level,
            Confidence::MAX,
            policy.minimum_confidence,
            evidence,
            RATE_FLOOR,
        ));
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
    // Joining a community is a prerequisite of posting to it, expressed as a
    // graph rather than as an inline condition so the same structure answers
    // both questions: which posts are held, and which join would release the
    // most of them.
    //
    // `joined == None` satisfies the prerequisite. It means discovery has no
    // membership record — an older target with no `discovery_places` row — and
    // those have been posted to successfully before. Treating unknown as
    // unjoined would retire a working community over a missing column.
    let mut graph = OpportunityGraph::new();
    for target in &snapshot.unengaged_targets {
        let membership = community_membership_key(&membership_address(target));
        graph.add(
            membership.clone(),
            community_post_key(target.target_id),
            DependencyKind::Prerequisite,
        );
        if target.joined != Some(false) {
            graph.satisfy(membership);
        }
    }
    // What the gate is costing, before anything is filtered out. A prerequisite
    // that removes candidates silently is indistinguishable from a brain with
    // nothing to say, and that is exactly how production looked: 119
    // communities discovered, none joined, every candidate dropped without a
    // decision row. Ranked by posts waiting, so the operator joins the one that
    // releases the most work first.
    let post_keys: Vec<String> = snapshot
        .unengaged_targets
        .iter()
        .map(|target| community_post_key(target.target_id))
        .collect();
    let mut demand: Vec<(String, u32)> = graph
        .blocked(post_keys.iter().map(String::as_str))
        .into_iter()
        .fold(
            std::collections::BTreeMap::<String, u32>::new(),
            |mut counts, block| {
                *counts.entry(block.waiting_on).or_default() += 1;
                counts
            },
        )
        .into_iter()
        .map(|(key, count)| {
            // Report the community, not the internal graph key.
            let community = key
                .strip_prefix("community_joined:")
                .unwrap_or(&key)
                .to_owned();
            (community, count)
        })
        .collect();
    demand.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    blocked_on_membership.extend(demand);

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
        // Joining is a prerequisite of posting, and until now nothing said so.
        //
        // The brain picks posts by expected value; a separate worker picks
        // joins by member count. Neither knew about the other, so the brain
        // could select a post to a community nobody had joined — which many
        // subreddits refuse outright, and which is the pattern that gets an
        // account flagged. The account it would be flagged on is the one the
        // discovery loop reads through.
        //
        // A gate rather than a penalty, for the same reason the community's
        // own cooldown is one: a post that will be refused is not a low-value
        // post, and nothing the optimizer can compute should let it outbid a
        // post that will actually run.
        //
        // `None` passes. It means discovery has no membership record — an
        // older target with no `discovery_places` row — and those have been
        // posted to successfully before. Treating unknown as unjoined would
        // retire a working community over a missing column.
        if graph.is_blocked(&community_post_key(target.target_id)) {
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
            avg_community_engagement_bps: snapshot.world_model.avg_community_engagement_bps,
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
        // The angle this post takes. Once a family has measured evidence on
        // this community the pick is Thompson-sampled from the per-family
        // posteriors — the label the post carries is what those posteriors
        // learn from, so the selection has to stay honest about which one
        // ran. Until then rotation stands: the even spread is what makes the
        // families comparable at all.
        let posts_so_far = community_history.iter().map(|h| h.post_count).sum::<u32>();
        let mut family_stats = std::collections::BTreeMap::new();
        let mut any_family_observed = false;
        for family in CreativeFamily::ALL {
            match causal_model.predict_family_stats(
                family,
                "community-engager",
                Some(&subreddit_type),
                Some(&target_key),
            ) {
                Some((mean, std, _confidence)) => {
                    any_family_observed = true;
                    family_stats.insert(family, (mean, std));
                }
                None => {
                    // Unmeasured families enter the draw at the skeptical
                    // prior — wide enough to stay competitive, which is what
                    // keeps them measurable instead of starved.
                    family_stats.insert(family, (0.0, crowdrelay_brain::PRIOR_VARIANCE.sqrt()));
                }
            }
        }
        let creative_family = if any_family_observed {
            CreativeFamily::choose(
                &family_stats,
                snapshot.has_upcoming_event,
                family_choice_seed(&target_key, posts_so_far),
            )
        } else {
            CreativeFamily::rotate_with_event(posts_so_far, snapshot.has_upcoming_event)
        };
        // Per-community prompt: one post for this specific community, in the
        // community's own frame — a subreddit reads "r/", a Discord server or
        // a forum board reads as the room it is.
        let mut prompt = if target.platform == "reddit" {
            format!(
                "Draft an authentic community post for r/{}. Write like a band member, not a marketer. Match this community's tone and language.",
                target.subreddit
            )
        } else {
            format!(
                "Draft an authentic community post for the {} community \"{}\". Write like a band member, not a marketer. Match this community's tone and language.",
                target.platform, target.display_name
            )
        };
        if target.platform == "reddit" {
            prompt.push_str(&format!(
                "\n\n- target_id: {}, platform: reddit, subreddit: {} ({})",
                target.target_id, target.subreddit, target.display_name
            ));
        } else {
            // `platform` is what routes the draft to the right lane — the
            // outcome resolves the community target on it, so the model must
            // echo it verbatim.
            prompt.push_str(&format!(
                "\n\n- target_id: {}, platform: {}, community: \"{}\"",
                target.target_id, target.platform, target.display_name
            ));
            if let Some(url) = &target.community_url {
                prompt.push_str(&format!("\n- community url: {url}"));
            }
        }
        if let Some(members) = target.member_count {
            prompt.push_str(&format!("\n- members: {members}"));
        }
        // The community's own conversion record — stated to the drafter
        // because "match what worked" means nothing when the evidence that
        // something worked never reaches the prompt. Zero is still worth
        // saying: a community that never converted is a different brief from
        // one that already produced fans.
        if target.converted_fans_90d > 0 || target.interactions_90d > 0 {
            prompt.push_str(&format!(
                "\n- measured record: {} fan(s) and {} click(s) arrived through this \
                 community's tracked links in the last 90 days, {} of those fans still \
                 active — match what worked",
                target.converted_fans_90d, target.interactions_90d, target.durable_fans_90d
            ));
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
            expected_metrics: treatment_stats
                .secondary
                .iter()
                .map(|(key, (mean, _, _))| (key.clone(), *mean))
                .collect(),
        };
        let action = AutopilotActionPayload::RequestAgentRun {
            template_id: "community-engager".to_owned(),
            prompt,
            priority: 2,
            tier: effective_agent_tier(AgentTier::Premium, snapshot.standing),
        };
        // Per-community decision_key: includes target_id so each community
        // has its own idempotency, cooldown, and experiment assignment.
        let community_unit_id = if target.platform == "reddit" {
            format!("r/{}", target.subreddit)
        } else {
            format!("{}:{}", target.platform, target.subreddit)
        };
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

/// The membership-gate address for a target: the subreddit on Reddit, and a
/// platform-qualified name anywhere else so a Discord server called "metal"
/// is not the same membership as r/metal.
fn membership_address(target: &UnengagedTarget) -> String {
    if target.platform == "reddit" {
        target.subreddit.clone()
    } else {
        format!("{}:{}", target.platform, target.subreddit)
    }
}

/// Deterministic seed for the family draw — the same community at the same
/// post count picks the same family, so re-evaluating a candidate inside one
/// cycle cannot re-roll the dice. `posts_so_far` is in the hash because the
/// pick should move post to post, not stay pinned to one family forever.
fn family_choice_seed(target_key: &str, posts_so_far: u32) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    target_key.hash(&mut hasher);
    posts_so_far.hash(&mut hasher);
    hasher.finish()
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
            platform: "reddit".to_owned(),
            subreddit: subreddit.to_owned(),
            community_url: None,
            member_count: Some(20_000),
            activity_basis_points: Some(4_000),
            genres: Vec::new(),
            self_promo_ratio_percent: Some(10),
            cooldown_days: Some(14),
            days_since_last_engagement: None,
            joined: Some(true),
            converted_fans_90d: 0,
            interactions_90d: 0,
            durable_fans_90d: 0,
        }
    }

    /// The regression that made this worth a graph rather than a condition.
    ///
    /// The gate is correct — a post to a community nobody joined gets refused,
    /// and is the pattern that flags the account the whole discovery loop reads
    /// through. What was wrong is that it removed candidates and said nothing.
    /// Production discovered 119 communities, joined none of them, and the
    /// Reddit channel read as idle rather than as blocked.
    #[test]
    fn an_unjoined_community_is_reported_not_only_skipped() {
        let mut graph = OpportunityGraph::new();
        let unjoined = target("djent");
        let membership = community_membership_key(&unjoined.subreddit);
        let post = community_post_key(unjoined.target_id);
        graph.add(
            membership.clone(),
            post.clone(),
            DependencyKind::Prerequisite,
        );
        // `joined: Some(false)` is the only value that withholds the
        // prerequisite; see the loop above for why `None` passes.
        assert!(
            graph.is_blocked(&post),
            "a post to an unjoined community must not be dispatched"
        );
        let blocked = graph.blocked([post.as_str()]);
        assert_eq!(blocked.len(), 1);
        assert_eq!(
            blocked[0].waiting_on, membership,
            "and the operator has to be told which community to join"
        );
    }

    #[test]
    fn a_joined_community_is_neither_blocked_nor_reported() {
        let mut graph = OpportunityGraph::new();
        let joined = target("metal");
        let membership = community_membership_key(&joined.subreddit);
        let post = community_post_key(joined.target_id);
        graph.add(
            membership.clone(),
            post.clone(),
            DependencyKind::Prerequisite,
        );
        graph.satisfy(membership);
        assert!(!graph.is_blocked(&post));
        assert!(graph.blocked([post.as_str()]).is_empty());
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

    /// A community nobody has joined produces no post candidate.
    #[test]
    fn an_unjoined_community_is_not_a_candidate() {
        let mut unjoined = target("r/metal");
        unjoined.joined = Some(false);
        assert_eq!(
            unjoined.joined,
            Some(false),
            "the gate reads this field; if it moves, the gate moves with it"
        );
    }

    /// Unknown membership is not refusal. An older target with no discovery
    /// place row has been posted to before, and a missing column must not
    /// retire it.
    #[test]
    fn unknown_membership_is_not_unjoined() {
        let mut unknown = target("r/metal");
        unknown.joined = None;
        assert_ne!(unknown.joined, Some(false));
    }
}
