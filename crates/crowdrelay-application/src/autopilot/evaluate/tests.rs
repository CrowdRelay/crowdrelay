#[cfg(test)]
mod tests {
    use super::*;
    use crowdrelay_domain::{
        EventId, ReleasePlanId, TeamOpportunityId, TicketTypeId,
        live_opportunities::{
            LiveOpportunityKind, LiveOpportunityPolicy, LiveOpportunitySnapshot,
            live_opportunity_score,
        },
        autonomy::{AutonomyLevel, Confidence, PolicyDisposition},
        pricing::TicketYieldPolicy,
        release_autopilot::{
            ReleaseAutopilotPolicy, ReleaseMilestone, ReleaseMilestoneHistory,
            ReleasePlanSnapshot, ReleaseTier, ShowWeekCollision,
        },
    };

    #[test]
    fn recommend_policy_never_creates_auto_execute_disposition()
    -> Result<(), Box<dyn std::error::Error>> {
        let minimum = Confidence::from_basis_points(8_000)?;
        let policy = AutopilotPolicy {
            context: AutopilotContext::TicketYield,
            enabled: true,
            autonomy_level: AutonomyLevel::Recommend,
            minimum_confidence: minimum,
            max_actions_24h: 10,
            config: AutopilotPolicyConfig::TicketYield(TicketYieldPolicy::default()),
            version: 1,
            guarded_until: None,
            guardrail_reason: None,
        };
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let candidate = ticket_candidate(
            TicketYieldSnapshot {
                ticket_type_id: TicketTypeId::new(),
                current_price_minor: 3_000,
                paid_quantity: 80,
                capacity: 100,
                sale_capacity: 100,
                paid_last_72h: 8,
                days_to_event: 21,
                last_price_change_at: None,
                last_capacity_change_at: None,
                allocation_guardrail: None,
            },
            &policy,
            now,
        )?
        .ok_or_else(|| std::io::Error::other("candidate expected"))?;
        assert_eq!(candidate.disposition, PolicyDisposition::RecommendOnly);
        Ok(())
    }
    /// A Landmark festival whose score lands in the gap between the configured
    /// floor and the real one.
    ///
    /// Strategic value 85% makes it Landmark (21 of 25 points), fit 70% gives 21
    /// of 30, reputation 60% gives 9 of 15, evidence 70% gives 10 of 15, and a
    /// bounded loss gives 6 of 15 economics — 67. With the default
    /// `minimum_score` of 65 the domain gate passes it; confidence is
    /// `7_500 + (67 - 65) * 100 = 7_700`, which is under the 8000 minimum, so the
    /// authority gate used to deny it.
    fn landmark_scoring_67() -> LiveOpportunitySnapshot {
        LiveOpportunitySnapshot {
            opportunity_id: TeamOpportunityId::new(),
            kind: LiveOpportunityKind::Festival,
            active: true,
            verified_destination: true,
            auto_submission_capable: true,
            fit_basis_points: 7_000,
            reputation_basis_points: 6_000,
            evidence_confidence: Confidence::from_basis_points(7_000)
                .expect("a valid confidence"),
            expected_fee_minor: 70_000,
            estimated_cost_minor: 90_000,
            application_fee_minor: 0,
            requires_contract: false,
            exclusive: false,
            deadline: None,
            event_starts_at: None,
            travel_band: None,
            costed_from_logistics: true,
            committed_shows_year: 0,
            pipeline_shows_year: 0,
            annual_target: 15,
            annual_stretch: 20,
            stretch_minimum_score_basis_points: 9_000,
            far_shot_minimum_score_basis_points: 9_000,
            prefer_weekend_one_shots: false,
            already_applied: false,
            strategic_value_basis_points: 8_500,
        }
    }

    fn live_policy(level: AutonomyLevel) -> Result<AutopilotPolicy, Box<dyn std::error::Error>> {
        Ok(AutopilotPolicy {
            context: AutopilotContext::LiveOpportunity,
            enabled: true,
            autonomy_level: level,
            minimum_confidence: Confidence::from_basis_points(8_000)?,
            max_actions_24h: 10,
            config: AutopilotPolicyConfig::LiveOpportunity(LiveOpportunityPolicy::default()),
            version: 1,
            guarded_until: None,
            guardrail_reason: None,
        })
    }

    /// The confidence gate must not decide that nobody sees a decision the
    /// domain gate sent to a person.
    ///
    /// `Deny` writes the decision to the ledger and creates no action row, so the
    /// opportunity never enters `awaiting_approval` — the queue `ops/attention`
    /// reads and the only one the operator looks at. Finding it afterwards means
    /// querying for `disposition = 'deny'`.
    #[test]
    fn a_score_between_the_configured_floor_and_the_real_one_still_reaches_a_human()
    -> Result<(), Box<dyn std::error::Error>> {
        let policy = live_policy(AutonomyLevel::RequireApproval)?;
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let snapshot = landmark_scoring_67();
        assert_eq!(
            live_opportunity_score(snapshot),
            67,
            "the fixture must land in the gap this test is about"
        );
        let candidate = live_opportunity_candidate(snapshot, &policy, now)?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;
        assert_eq!(
            candidate.disposition,
            PolicyDisposition::RequireApproval,
            "a verified opportunity above the configured minimum_score must reach \
             the approval queue rather than being denied by the confidence gate"
        );
        Ok(())
    }

    /// The lift obeys the autonomy level, which is the trap in fixing this.
    ///
    /// `disposition` tests confidence before the level, so a low-confidence
    /// decision returns `Deny` on an `Observe` workspace too. Rewriting that to
    /// `RequireApproval` would put approval requests in front of an operator who
    /// asked the autopilot to observe and nothing else.
    #[test]
    fn the_lift_does_not_promote_an_observe_only_workspace()
    -> Result<(), Box<dyn std::error::Error>> {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let snapshot = landmark_scoring_67();
        for (level, expected) in [
            (AutonomyLevel::Observe, PolicyDisposition::ObserveOnly),
            (AutonomyLevel::Recommend, PolicyDisposition::RecommendOnly),
        ] {
            let policy = live_policy(level)?;
            let candidate = live_opportunity_candidate(snapshot, &policy, now)?
                .ok_or_else(|| std::io::Error::other("candidate expected"))?;
            assert_eq!(
                candidate.disposition, expected,
                "{level:?} must keep its own disposition, not be promoted to \
                 approval by the Deny lift"
            );
        }
        Ok(())
    }

    /// And it still never widens what the machine may do unattended.
    #[test]
    fn a_forced_approval_decision_never_auto_executes()
    -> Result<(), Box<dyn std::error::Error>> {
        let policy = live_policy(AutonomyLevel::BoundedAuto)?;
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let snapshot = landmark_scoring_67();
        let candidate = live_opportunity_candidate(snapshot, &policy, now)?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;
        assert_eq!(
            candidate.disposition,
            PolicyDisposition::RequireApproval,
            "the domain routed this to a human; bounded-auto does not override that"
        );
        Ok(())
    }

    /// Finds the real confidence floor by asking the production evaluator.
    ///
    /// `minimum_confidence` is a score floor wearing different units. Live
    /// opportunity confidence is `7_500 + (score - minimum_score) * 100`, so with
    /// a 8000 minimum the first score to clear it is `minimum_score + 5` — an
    /// operator who sets `minimum_score` to 65 has really set 70, and the two
    /// numbers live in different crates with neither mentioning the other.
    ///
    /// Nothing is lost to it now: a decision the domain routed to a human is
    /// lifted past the confidence gate, because that gate decides whether the
    /// *machine* may act alone. The gap still governs whether an opportunity can
    /// ever auto-submit, so it is worth having written down.
    ///
    /// The first version of this test computed the formula inline and asserted it
    /// against itself. Moving the real base from 7_500 to 8_000 did not fail it.
    /// This one sweeps `fit_basis_points` through `evaluate_live_opportunity` and
    /// reads the confidence the production path actually produced, so the
    /// assertion is about the code rather than about a copy of it.
    #[test]
    fn the_confidence_floor_is_a_second_score_floor() -> Result<(), Box<dyn std::error::Error>> {
        let minimum = Confidence::from_basis_points(8_000)?;
        let policy = LiveOpportunityPolicy::default();
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);

        let mut lowest_clearing: Option<u16> = None;
        let mut highest_denied: Option<u16> = None;
        for fit in (0..=10_000_u16).step_by(100) {
            let mut snapshot = landmark_scoring_67();
            snapshot.fit_basis_points = fit;
            let score = live_opportunity_score(snapshot);
            let confidence = match evaluate_live_opportunity(snapshot, policy, now) {
                LiveOpportunityDecision::PrepareForApproval { confidence, .. }
                | LiveOpportunityDecision::SubmitAutomatically { confidence, .. }
                | LiveOpportunityDecision::EscalateLandmark { confidence, .. } => confidence,
                // Below `minimum_score`, or refused for another reason. The
                // confidence gate is not what is being measured there.
                LiveOpportunityDecision::Hold => continue,
            };
            if confidence.basis_points() >= minimum.basis_points() {
                lowest_clearing = Some(lowest_clearing.map_or(score, |best| best.min(score)));
            } else {
                highest_denied = Some(highest_denied.map_or(score, |worst| worst.max(score)));
            }
        }

        let lowest_clearing = lowest_clearing.expect("some score must clear the floor");
        let highest_denied = highest_denied.expect("some score must fall under it");
        assert_eq!(
            lowest_clearing,
            policy.minimum_score + 5,
            "the evaluator's own confidence clears {} five points above the \
             configured minimum_score of {}",
            minimum.basis_points(),
            policy.minimum_score
        );
        assert_eq!(
            highest_denied,
            policy.minimum_score + 4,
            "and the band the confidence gate rejects runs right up to it, so \
             minimum_score is not the floor an operator gets"
        );
        Ok(())
    }
    fn release_policy() -> AutopilotPolicy {
        AutopilotPolicy {
            context: AutopilotContext::Release,
            enabled: true,
            autonomy_level: AutonomyLevel::Recommend,
            minimum_confidence: Confidence::from_basis_points(1)
                .unwrap_or_else(|_| Confidence::saturating_from_basis_points(1)),
            max_actions_24h: 10,
            config: AutopilotPolicyConfig::Release(ReleaseAutopilotPolicy::default()),
            version: 1,
            guarded_until: None,
            guardrail_reason: None,
        }
    }

    /// Ten days out with the calendar seeded and the pitch already done is the
    /// fan-warmup slot — an owned-audience send, which is the kind §4i-2 holds.
    fn warmup_due() -> (ReleasePlanSnapshot, OffsetDateTime) {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        (
            ReleasePlanSnapshot {
                release_id: ReleasePlanId::new(),
                title: "Signal Lost".to_string(),
                release_at: now + time::Duration::days(10),
                active: true,
                tier: ReleaseTier::Track,
                assets_ready: true,
                communication_enabled: true,
                press_enabled: true,
                editorial_pitch_completed_at: Some(now),
                editorial_pitch_escalated_at: None,
                tier_release_miss_streak: 0,
                history: ReleaseMilestoneHistory {
                    calendar_seeded: true,
                    ..ReleaseMilestoneHistory::default()
                },
            },
            now,
        )
    }

    fn show_this_week(now: OffsetDateTime, title: &str) -> ShowWeekCollision {
        ShowWeekCollision {
            event_id: EventId::new(),
            title: title.to_string(),
            starts_at: now + time::Duration::days(1),
            week_start: now.date()
                - time::Duration::days(i64::from(now.weekday().number_days_from_monday())),
        }
    }

    #[test]
    fn an_owned_audience_milestone_holds_when_the_week_already_has_a_show()
    -> Result<(), Box<dyn std::error::Error>> {
        let (snapshot, now) = warmup_due();
        let shows = [show_this_week(now, "Klub Hybrydy")];
        let candidate = release_candidate(snapshot, &release_policy(), now, &shows)?
            .ok_or_else(|| std::io::Error::other("held candidate expected"))?;
        assert_eq!(candidate.decision_kind, "hold_release_milestone_collision");
        assert_eq!(candidate.disposition, PolicyDisposition::Deny);
        // The decision names the moment it protected — the rule's receipt, not
        // a side channel.
        assert_eq!(
            candidate.input_snapshot["collision"]["protected_shows"][0]["title"],
            "Klub Hybrydy"
        );
        assert_eq!(
            candidate.input_snapshot["collision"]["held_milestone"],
            "fan_warmup"
        );
        assert!(candidate.decision_key.contains(":hold:"));
        Ok(())
    }

    #[test]
    fn every_show_in_the_collision_week_is_named()
    -> Result<(), Box<dyn std::error::Error>> {
        let (snapshot, now) = warmup_due();
        let shows = [
            show_this_week(now, "Friday gig"),
            show_this_week(now, "Sunday gig"),
        ];
        let candidate = release_candidate(snapshot, &release_policy(), now, &shows)?
            .ok_or_else(|| std::io::Error::other("held candidate expected"))?;
        assert_eq!(
            candidate.input_snapshot["collision"]["protected_shows"]
                .as_array()
                .map_or(0, Vec::len),
            2
        );
        Ok(())
    }

    #[test]
    fn a_held_milestone_offers_itself_again_under_the_ordinary_key()
    -> Result<(), Box<dyn std::error::Error>> {
        let (snapshot, now) = warmup_due();
        let held = release_candidate(
            snapshot.clone(),
            &release_policy(),
            now,
            &[show_this_week(now, "Klub Hybrydy")],
        )?
        .ok_or_else(|| std::io::Error::other("held candidate expected"))?;
        // The week cleared: no collision, same milestone, ordinary key — a
        // fresh decision row, not an amendment to the hold.
        let clear = release_candidate(snapshot, &release_policy(), now, &[])?
            .ok_or_else(|| std::io::Error::other("execute candidate expected"))?;
        assert_eq!(clear.decision_kind, "execute_release_milestone");
        assert_ne!(held.decision_key, clear.decision_key);
        assert!(!clear.decision_key.contains(":hold:"));
        assert_eq!(
            held.action_idempotency_key.split(":hold:").next(),
            Some(clear.action_idempotency_key.as_str())
        );
        Ok(())
    }

    #[test]
    fn press_and_internal_milestones_do_not_hold_for_a_show_week()
    -> Result<(), Box<dyn std::error::Error>> {
        let (mut snapshot, now) = warmup_due();
        let shows = [show_this_week(now, "Klub Hybrydy")];
        // Eighteen days out is the press window — third-party attention, not
        // the fans' week.
        snapshot.release_at = now + time::Duration::days(18);
        let press = release_candidate(snapshot.clone(), &release_policy(), now, &shows)?
            .ok_or_else(|| std::io::Error::other("press candidate expected"))?;
        assert_eq!(press.decision_kind, "execute_release_milestone");
        assert!(matches!(
            press.action,
            AutopilotActionPayload::ExecuteReleaseMilestone {
                milestone: ReleaseMilestone::StartPress,
                ..
            }
        ));
        // Thirty days out is the calendar seed — internal, reaches nobody.
        snapshot.release_at = now + time::Duration::days(30);
        snapshot.history.calendar_seeded = false;
        let calendar = release_candidate(snapshot, &release_policy(), now, &shows)?
            .ok_or_else(|| std::io::Error::other("calendar candidate expected"))?;
        assert_eq!(calendar.decision_kind, "execute_release_milestone");
        assert!(matches!(
            calendar.action,
            AutopilotActionPayload::ExecuteReleaseMilestone {
                milestone: ReleaseMilestone::SeedCalendar,
                ..
            }
        ));
        Ok(())
    }

    #[test]
    fn a_milestone_already_sent_never_enters_the_collision_check()
    -> Result<(), Box<dyn std::error::Error>> {
        let (mut snapshot, now) = warmup_due();
        snapshot.history.fan_warmup_sent = true;
        let shows = [show_this_week(now, "Klub Hybrydy")];
        assert!(release_candidate(snapshot, &release_policy(), now, &shows)?.is_none());
        Ok(())
    }

    /// The tier's own R+14 ledger answering for itself: two releases in a row
    /// that showed no lift means the next outward send at this tier earns a
    /// human look rather than another automatic spend of audience attention.
    /// Internal rungs keep running — they cost nobody's attention.
    #[test]
    fn a_tier_that_keeps_missing_demotes_outward_sends_to_approval()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut auto = release_policy();
        auto.autonomy_level = AutonomyLevel::BoundedAuto;

        let (mut snapshot, now) = warmup_due();
        snapshot.tier_release_miss_streak = 2;
        let warmup = release_candidate(snapshot.clone(), &auto, now, &[])?
            .ok_or_else(|| std::io::Error::other("warmup candidate expected"))?;
        assert_eq!(warmup.disposition, PolicyDisposition::RequireApproval);
        assert!(warmup.reason.contains("no lift"));

        // The same policy without the streak stays automatic — the demotion
        // is earned by the tier's outcomes, not assumed.
        snapshot.tier_release_miss_streak = 0;
        let clean = release_candidate(snapshot.clone(), &auto, now, &[])?
            .ok_or_else(|| std::io::Error::other("warmup candidate expected"))?;
        assert_eq!(clean.disposition, PolicyDisposition::AutoExecute);

        // And the calendar seed is internal — a miss streak parks no
        // workspace bookkeeping.
        snapshot.tier_release_miss_streak = 2;
        snapshot.release_at = now + time::Duration::days(30);
        snapshot.history.calendar_seeded = false;
        let calendar = release_candidate(snapshot, &auto, now, &[])?
            .ok_or_else(|| std::io::Error::other("calendar candidate expected"))?;
        assert_eq!(calendar.disposition, PolicyDisposition::AutoExecute);
        Ok(())
    }

    #[test]
    fn a_retried_artifact_gets_its_own_keys() -> Result<(), Box<dyn std::error::Error>> {
        use crowdrelay_domain::{
            ContentSourceId,
            content_supply::{
                ContentArtifactKind, ContentSupplyPolicy, ContentSupplySnapshot, FailedArtifact,
            },
        };
        let now = OffsetDateTime::now_utc();
        let mut snapshot = ContentSupplySnapshot {
            source_id: ContentSourceId::new(),
            source_kind: ContentSourceKind::Event,
            source_version: 2,
            occurred_at: now - time::Duration::days(1),
            expires_at: now + time::Duration::days(10),
            communication_enabled: None,
            press_enabled: None,
            release_tier: None,
            completed_artifacts: Vec::new(),
            in_flight_artifacts: Vec::new(),
            failed_artifacts: Vec::new(),
            social_post: None,
        };
        let policy = AutopilotPolicy {
            context: AutopilotContext::ContentSupply,
            enabled: true,
            autonomy_level: AutonomyLevel::BoundedAuto,
            minimum_confidence: Confidence::from_basis_points(5_000)?,
            max_actions_24h: 10,
            config: AutopilotPolicyConfig::ContentSupply(ContentSupplyPolicy::default()),
            version: 1,
            guarded_until: None,
            guardrail_reason: None,
        };
        let keys = |snapshot: &ContentSupplySnapshot| -> Result<(String, String), serde_json::Error> {
            let candidates =
                content_candidates(snapshot, &policy, &[], None, ContextEvidence::UNPROVEN, now)?;
            assert_eq!(candidates.len(), 1);
            Ok((
                candidates[0].decision_key.clone(),
                candidates[0].action_idempotency_key.clone(),
            ))
        };

        // The first request keeps the key it always had, so actions already
        // written stay deduplicated against it.
        let (first_decision, first_action) = keys(&snapshot)?;
        let source = snapshot.source_id;
        assert_eq!(first_action, format!("action:content:{source}:sv2:LiveListing"));

        snapshot.failed_artifacts = vec![FailedArtifact {
            artifact: ContentArtifactKind::LiveListing,
            failures: 1,
            last_failed_at: now - time::Duration::hours(1),
        }];
        let (retry_decision, retry_action) = keys(&snapshot)?;
        assert_eq!(retry_action, format!("{first_action}:attempt1"));
        assert_eq!(retry_decision, format!("{first_decision}:attempt1"));
        Ok(())
    }

    #[test]
    fn a_fresh_synced_post_relays_to_owned_channel_and_admitted_communities()
    -> Result<(), Box<dyn std::error::Error>> {
        use crowdrelay_domain::{
            ContentSourceId, OutreachTargetId,
            content_supply::{
                CommunityRelayTarget, ContentSupplyPolicy, ContentSupplySnapshot, SocialPostFact,
            },
        };
        let now = OffsetDateTime::now_utc();
        let snapshot = ContentSupplySnapshot {
            source_id: ContentSourceId::new(),
            source_kind: ContentSourceKind::SocialPost,
            source_version: 3,
            // Settled (past the 36h resonance window) and above the account's
            // own median — the post communities should get.
            occurred_at: now - time::Duration::hours(40),
            expires_at: now + time::Duration::days(39),
            communication_enabled: None,
            press_enabled: None,
            release_tier: None,
            completed_artifacts: Vec::new(),
            in_flight_artifacts: Vec::new(),
            failed_artifacts: Vec::new(),
            social_post: Some(SocialPostFact {
                title: "soundcheck done".to_owned(),
                url: Some("https://instagram.com/p/abc".to_owned()),
                platform: "instagram".to_owned(),
                body: Some("soundcheck done — see you at the show".to_owned()),
                media_url: Some("https://cdn.instagram.example/img.jpg".to_owned()),
                media_id: Some("1790".to_owned()),
                media_type: Some("IMAGE".to_owned()),
                thumbnail_url: None,
                resonance: Some(crowdrelay_domain::content_supply::PostResonance {
                    engagement: 90,
                    peer_median: Some(40),
                    peers: 12,
                    ..Default::default()
                }),
            }),
        };
        let policy = AutopilotPolicy {
            context: AutopilotContext::ContentSupply,
            enabled: true,
            autonomy_level: AutonomyLevel::BoundedAuto,
            minimum_confidence: Confidence::from_basis_points(5_000)?,
            max_actions_24h: 10,
            config: AutopilotPolicyConfig::ContentSupply(ContentSupplyPolicy::default()),
            version: 1,
            guarded_until: None,
            guardrail_reason: None,
        };
        let communities = vec![
            CommunityRelayTarget {
                target_id: OutreachTargetId::new(),
                subreddit: "indieheads".to_owned(),
                language: Some("en".to_owned()),
            },
            CommunityRelayTarget {
                target_id: OutreachTargetId::new(),
                subreddit: "listentothis".to_owned(),
                language: None,
            },
        ];

        // The approval quotes the audience the send would deliver — here the
        // workspace's per-step envelope caps a bigger eligible set.
        let push_audience = Some(crowdrelay_domain::content_supply::SignalPushAudience {
            eligible: 120,
            reached: 40,
        });

        let candidates =
            content_candidates(
            &snapshot,
            &policy,
            &communities,
            push_audience,
            ContextEvidence::UNPROVEN,
            now,
        )?;
        assert_eq!(candidates.len(), 3);

        // The owned-channel carry is a push, not a new broadcast.
        match &candidates[0].action {
            AutopilotActionPayload::RequestSignalPush {
                title,
                body,
                audience_size,
                audience_basis,
                ..
            } => {
                assert_eq!(title, "soundcheck done");
                assert!(body.contains("soundcheck done — see you at the show"));
                assert!(body.contains("https://instagram.com/p/abc"));
                assert_eq!(*audience_size, Some(40));
                assert!(audience_basis.contains("caps this push at 40"));
            }
            other => return Err(format!("expected signal push, got {other:?}").into()),
        }
        assert_eq!(
            candidates[0].action.action_class(),
            ActionClass::OwnedAudience
        );

        // Each admitted community gets a repost draft task — the post itself
        // is written by the repost worker in the community's own language and
        // lands on the approval queue as the composed text, not a caption
        // dump. The brain picks the (post × community) pairs; the worker
        // only writes.
        for (candidate, target) in candidates[1..].iter().zip(&communities) {
            match &candidate.action {
                AutopilotActionPayload::RequestAgentRun {
                    template_id,
                    prompt,
                    tier,
                    ..
                } => {
                    assert_eq!(template_id, "community-repost");
                    assert!(prompt.contains(&format!("r/{}", target.subreddit)));
                    assert!(prompt.contains(&target.target_id.into_uuid().to_string()));
                    assert!(prompt.contains("soundcheck done — see you at the show"));
                    assert!(prompt.contains("https://instagram.com/p/abc"));
                    // The recorded language is handed to the drafter; an
                    // unrecorded one is named as such, not guessed silently.
                    match target.language.as_deref() {
                        Some(language) => {
                            assert!(prompt.contains(&format!("language: {language}")));
                        }
                        None => assert!(prompt.contains("not recorded")),
                    }
                    // A caption adaptation is free-tier work.
                    assert_eq!(*tier, crowdrelay_brain::AgentTier::Basic);
                }
                other => {
                    return Err(format!("expected repost draft task, got {other:?}").into())
                }
            }
            assert_eq!(candidate.decision_kind, "relay_owned_post");
            // The community is the subject — the inflight-subject index must
            // see N community relays as N different subjects, or the second
            // onward folds into the first and one post reaches one community.
            assert_eq!(
                candidate.subject,
                crate::autopilot::model::ActionSubject::TargetCommunity(
                    target.target_id.into_uuid()
                )
            );
        }

        // A caption edit bumps the source version; the relay keys stay put, so
        // the same post can never be carried twice.
        let mut edited = snapshot.clone();
        edited.source_version = 4;
        let again = content_candidates(
            &edited,
            &policy,
            &communities,
            push_audience,
            ContextEvidence::UNPROVEN,
            now,
        )?;
        for (first, second) in candidates.iter().zip(&again) {
            assert_eq!(first.action_idempotency_key, second.action_idempotency_key);
            assert_eq!(first.decision_key, second.decision_key);
        }
        Ok(())
    }

    #[test]
    fn a_relay_with_no_admitted_communities_still_reaches_the_owned_channel()
    -> Result<(), Box<dyn std::error::Error>> {
        use crowdrelay_domain::{
            ContentSourceId,
            content_supply::{ContentSupplyPolicy, ContentSupplySnapshot, SocialPostFact},
        };
        let now = OffsetDateTime::now_utc();
        let snapshot = ContentSupplySnapshot {
            source_id: ContentSourceId::new(),
            source_kind: ContentSourceKind::SocialPost,
            source_version: 1,
            occurred_at: now - time::Duration::hours(1),
            expires_at: now + time::Duration::days(44),
            communication_enabled: None,
            press_enabled: None,
            release_tier: None,
            completed_artifacts: Vec::new(),
            in_flight_artifacts: Vec::new(),
            failed_artifacts: Vec::new(),
            social_post: Some(SocialPostFact {
                title: "new demo up".to_owned(),
                url: None,
                platform: "facebook".to_owned(),
                body: None,
                media_url: None,
                media_id: None,
                media_type: None,
                thumbnail_url: None,
                resonance: None,
            }),
        };
        let policy = AutopilotPolicy {
            context: AutopilotContext::ContentSupply,
            enabled: true,
            autonomy_level: AutonomyLevel::BoundedAuto,
            minimum_confidence: Confidence::from_basis_points(5_000)?,
            max_actions_24h: 10,
            config: AutopilotPolicyConfig::ContentSupply(ContentSupplyPolicy::default()),
            version: 1,
            guarded_until: None,
            guardrail_reason: None,
        };

        let candidates = content_candidates(&snapshot, &policy, &[], None, ContextEvidence::UNPROVEN, now)?;
        assert_eq!(candidates.len(), 1);
        assert!(matches!(
            candidates[0].action,
            AutopilotActionPayload::RequestSignalPush { .. }
        ));
        Ok(())
    }



    #[test]
    fn a_post_that_has_not_landed_at_home_reaches_fans_but_not_communities()
    -> Result<(), Box<dyn std::error::Error>> {
        use crowdrelay_domain::{
            ContentSourceId, OutreachTargetId,
            content_supply::{
                CommunityRelayTarget, ContentSupplyPolicy, ContentSupplySnapshot, PostResonance,
                SocialPostFact,
            },
        };
        let now = OffsetDateTime::now_utc();
        let snapshot = |hours_old: i64, engagement: i64| ContentSupplySnapshot {
            source_id: ContentSourceId::new(),
            source_kind: ContentSourceKind::SocialPost,
            source_version: 1,
            occurred_at: now - time::Duration::hours(hours_old),
            expires_at: now + time::Duration::days(40),
            communication_enabled: None,
            press_enabled: None,
            release_tier: None,
            completed_artifacts: Vec::new(),
            in_flight_artifacts: Vec::new(),
            failed_artifacts: Vec::new(),
            social_post: Some(SocialPostFact {
                title: "rehearsal cut".to_owned(),
                url: Some("https://instagram.com/p/xyz".to_owned()),
                platform: "instagram".to_owned(),
                body: Some("rehearsal cut of the new one".to_owned()),
                media_url: None,
                media_id: None,
                media_type: None,
                thumbnail_url: None,
                resonance: Some(PostResonance {
                    engagement,
                    peer_median: Some(40),
                    peers: 12,
                    ..Default::default()
                }),
            }),
        };
        let policy = AutopilotPolicy {
            context: AutopilotContext::ContentSupply,
            enabled: true,
            autonomy_level: AutonomyLevel::BoundedAuto,
            minimum_confidence: Confidence::from_basis_points(5_000)?,
            max_actions_24h: 10,
            config: AutopilotPolicyConfig::ContentSupply(ContentSupplyPolicy::default()),
            version: 1,
            guarded_until: None,
            guardrail_reason: None,
        };
        let communities = vec![CommunityRelayTarget {
            target_id: OutreachTargetId::new(),
            subreddit: "doommetal".to_owned(),
            language: Some("en".to_owned()),
        }];
        // Five hours old: doing well, but too early to tell.
        let fresh = content_candidates(
            &snapshot(5, 90),
            &policy,
            &communities,
            None,
            ContextEvidence::UNPROVEN,
            now,
        )?;
        // Settled, below the account's own median.
        let weak = content_candidates(
            &snapshot(40, 10),
            &policy,
            &communities,
            None,
            ContextEvidence::UNPROVEN,
            now,
        )?;
        for candidates in [fresh, weak] {
            assert_eq!(candidates.len(), 1, "owned push only");
            assert!(matches!(
                candidates[0].action,
                AutopilotActionPayload::RequestSignalPush { .. }
            ));
        }
        Ok(())
    }

    /// A post that says "tonight" is not pushed a day and a half later —
    /// fans were pushed "dzisiaj wieczorne słuchowisko" two days late on
    /// 2026-09-26. The communities still get their rewritten drafts.
    #[test]
    fn a_post_about_tonight_is_not_pushed_once_the_night_has_passed()
    -> Result<(), Box<dyn std::error::Error>> {
        use crowdrelay_domain::{
            ContentSourceId, OutreachTargetId,
            content_supply::{
                CommunityRelayTarget, ContentSupplyPolicy, ContentSupplySnapshot, SocialPostFact,
            },
        };
        let now = OffsetDateTime::now_utc();
        let snapshot = ContentSupplySnapshot {
            source_id: ContentSourceId::new(),
            source_kind: ContentSourceKind::SocialPost,
            source_version: 3,
            // Settled (past the 36h resonance window) and above the account's
            // own median — the post communities should get.
            occurred_at: now - time::Duration::hours(40),
            expires_at: now + time::Duration::days(39),
            communication_enabled: None,
            press_enabled: None,
            release_tier: None,
            completed_artifacts: Vec::new(),
            in_flight_artifacts: Vec::new(),
            failed_artifacts: Vec::new(),
            social_post: Some(SocialPostFact {
                title: "soundcheck done".to_owned(),
                url: Some("https://instagram.com/p/abc".to_owned()),
                platform: "instagram".to_owned(),
                body: Some("soundcheck done — see you tonight".to_owned()),
                media_url: Some("https://cdn.instagram.example/img.jpg".to_owned()),
                media_id: Some("1790".to_owned()),
                media_type: Some("IMAGE".to_owned()),
                thumbnail_url: None,
                resonance: Some(crowdrelay_domain::content_supply::PostResonance {
                    engagement: 90,
                    peer_median: Some(40),
                    peers: 12,
                    ..Default::default()
                }),
            }),
        };
        let policy = AutopilotPolicy {
            context: AutopilotContext::ContentSupply,
            enabled: true,
            autonomy_level: AutonomyLevel::BoundedAuto,
            minimum_confidence: Confidence::from_basis_points(5_000)?,
            max_actions_24h: 10,
            config: AutopilotPolicyConfig::ContentSupply(ContentSupplyPolicy::default()),
            version: 1,
            guarded_until: None,
            guardrail_reason: None,
        };
        let communities = vec![
            CommunityRelayTarget {
                target_id: OutreachTargetId::new(),
                subreddit: "indieheads".to_owned(),
                language: Some("en".to_owned()),
            },
            CommunityRelayTarget {
                target_id: OutreachTargetId::new(),
                subreddit: "listentothis".to_owned(),
                language: None,
            },
        ];

        let candidates = content_candidates(
            &snapshot,
            &policy,
            &communities,
            None,
            ContextEvidence::UNPROVEN,
            now,
        )?;
        assert!(
            candidates
                .iter()
                .all(|candidate| !matches!(candidate.action, AutopilotActionPayload::RequestSignalPush { .. })),
            "no push for a passed tonight"
        );
        assert_eq!(candidates.len(), 2, "the two community drafts remain");
        Ok(())
    }
}
