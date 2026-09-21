#[cfg(test)]
mod tests {
    use super::*;
    use crowdrelay_domain::{
        BookingTargetId, CityId, EventId, ReleasePlanId, TeamOpportunityId, TicketTypeId,
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
            occurred_at: now - time::Duration::hours(5),
            expires_at: now + time::Duration::days(39),
            communication_enabled: None,
            press_enabled: None,
            release_tier: None,
            completed_artifacts: Vec::new(),
            in_flight_artifacts: Vec::new(),
            social_post: Some(SocialPostFact {
                title: "soundcheck done".to_owned(),
                url: Some("https://instagram.com/p/abc".to_owned()),
                platform: "instagram".to_owned(),
                body: Some("soundcheck done — see you tonight".to_owned()),
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
            },
            CommunityRelayTarget {
                target_id: OutreachTargetId::new(),
                subreddit: "listentothis".to_owned(),
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
            EvidenceCount::NONE,
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
                assert!(body.contains("soundcheck done — see you tonight"));
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

        // Each admitted community gets the band's own words, attributed.
        for (candidate, target) in candidates[1..].iter().zip(&communities) {
            match &candidate.action {
                AutopilotActionPayload::RequestCommunityEngagement {
                    target_id,
                    subreddit,
                    title,
                    body,
                    ..
                } => {
                    assert_eq!(*target_id, target.target_id.into_uuid());
                    assert_eq!(subreddit.as_deref(), Some(target.subreddit.as_str()));
                    assert_eq!(title, "soundcheck done");
                    assert!(body.contains("Originally posted on instagram"));
                    assert!(body.contains("https://instagram.com/p/abc"));
                }
                other => {
                    return Err(format!("expected community engagement, got {other:?}").into())
                }
            }
            assert_eq!(candidate.action.action_class(), ActionClass::ThirdParty);
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
            EvidenceCount::NONE,
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
            social_post: Some(SocialPostFact {
                title: "new demo up".to_owned(),
                url: None,
                platform: "facebook".to_owned(),
                body: None,
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

        let candidates = content_candidates(&snapshot, &policy, &[], None, EvidenceCount::NONE, now)?;
        assert_eq!(candidates.len(), 1);
        assert!(matches!(
            candidates[0].action,
            AutopilotActionPayload::RequestSignalPush { .. }
        ));
        Ok(())
    }

    // ---- §12-6: booking proposal carries room, window and recipient set ----

    fn booking_policy() -> Result<AutopilotPolicy, Box<dyn std::error::Error>> {
        use crowdrelay_domain::booking::BookingOpportunityPolicy;
        Ok(AutopilotPolicy {
            context: AutopilotContext::BookingOpportunity,
            enabled: true,
            autonomy_level: AutonomyLevel::RequireApproval,
            minimum_confidence: Confidence::from_basis_points(5_000)?,
            max_actions_24h: 10,
            config: AutopilotPolicyConfig::BookingOpportunity(
                BookingOpportunityPolicy::default(),
            ),
            version: 1,
            guarded_until: None,
            guardrail_reason: None,
        })
    }

    fn booking_city(city_id: CityId) -> CityOpportunitySnapshot {
        CityOpportunitySnapshot {
            city_id,
            active_fans: 90,
            new_fans_30d: 20,
            event_interests: 40,
            area_claims: 10,
            months_since_last_show: Some(12),
            market_evidence: None,
            outreach_in_flight: false,
            last_outreach_at: None,
        }
    }

    fn booking_target(
        city_id: CityId,
        priority: u16,
        venue_evidence: Option<crowdrelay_domain::booking::BookingVenueEvidence>,
    ) -> BookingTargetSnapshot {
        BookingTargetSnapshot {
            target_id: BookingTargetId::new(),
            city_id,
            kind: crowdrelay_domain::booking::BookingTargetKind::Venue,
            display_name: "Klub Test".to_owned(),
            capacity: Some(200),
            version: 1,
            active: true,
            accepts_booking: true,
            priority,
            relationship_score: 60,
            outreach_in_flight: false,
            last_outreach_at: None,
            followup_count: 0,
            last_reply: crowdrelay_domain::booking::BookingReplyDisposition::None,
            venue_evidence,
            days_until_application_close: None,
            next_application_closes_at: None,
            linked_venue_ids: Vec::new(),
        }
    }

    #[test]
    fn booking_candidate_carries_window_recipients_and_evidence()
    -> Result<(), Box<dyn std::error::Error>> {
        use crowdrelay_domain::booking::BookingVenueEvidence;
        use crowdrelay_domain::booking_window::{
            BookingWindowInputSet, BookingWindowTargetInputs,
        };
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let city_id = CityId::new();
        let evidence = BookingVenueEvidence {
            shows_last_12m: 9,
            comparable_acts: 3,
            genres: Some("metal".to_owned()),
            capacity: None,
            days_since_last_event: Some(11),
            booking_contact_days: None,
        };
        let anchor = booking_target(city_id, 90, Some(evidence.clone()));
        let second = booking_target(city_id, 70, None);
        let third = booking_target(city_id, 60, None);
        // Beyond the cap of two extras — should not be picked.
        let fourth = booking_target(city_id, 50, None);
        let targets = vec![
            anchor.clone(),
            second.clone(),
            third.clone(),
            fourth.clone(),
        ];
        let mut window_inputs = BookingWindowInputSet::default();
        // Two room shows 28 days apart, each booked ~42 days ahead.
        let show = |days_ago: i64, lag: i64| {
            let starts_at = now - time::Duration::days(days_ago);
            (starts_at, starts_at - time::Duration::days(lag))
        };
        window_inputs.targets.push(BookingWindowTargetInputs {
            target_id: anchor.target_id,
            room_shows: vec![show(20, 42), show(48, 42)],
            venue_coords: None,
        });

        let candidate = booking_candidate(
            booking_city(city_id),
            &targets,
            &window_inputs,
            &booking_policy()?,
            now,
        )?
        .ok_or_else(|| std::io::Error::other("a candidate is expected"))?;

        let AutopilotActionPayload::RequestBookingOutreach {
            target_id,
            proposed_window,
            additional_recipients,
            venue_evidence,
            ..
        } = &candidate.action
        else {
            return Err("expected RequestBookingOutreach".into());
        };
        assert_eq!(*target_id, anchor.target_id);
        let window = proposed_window
            .as_ref()
            .ok_or_else(|| std::io::Error::other("a window is expected"))?;
        assert_eq!(
            window.start,
            now.date() + time::Duration::days(42),
            "median lead time sets the ask",
        );
        assert_eq!(
            additional_recipients.as_slice(),
            &[(second.target_id, 1), (third.target_id, 1)],
        );
        assert_eq!(venue_evidence.as_ref(), Some(&evidence));
        assert_eq!(candidate.action.action_class(), ActionClass::ThirdParty);
        Ok(())
    }

    #[test]
    fn booking_decision_key_tracks_window_and_recipients()
    -> Result<(), Box<dyn std::error::Error>> {
        use crowdrelay_domain::booking_window::{
            BookingWindowInputSet, BookingWindowTargetInputs,
        };
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let city_id = CityId::new();
        let anchor = booking_target(city_id, 90, None);
        let extra = booking_target(city_id, 70, None);
        let targets = vec![anchor.clone(), extra.clone()];
        let policy = booking_policy()?;
        let empty_inputs = BookingWindowInputSet::default();

        let without_window =
            booking_candidate(booking_city(city_id), &targets, &empty_inputs, &policy, now)?
                .ok_or_else(|| std::io::Error::other("candidate expected"))?;
        // No window inputs → no window; the second target still joins.
        let AutopilotActionPayload::RequestBookingOutreach {
            proposed_window,
            additional_recipients,
            ..
        } = &without_window.action
        else {
            return Err("expected RequestBookingOutreach".into());
        };
        assert_eq!(*proposed_window, None);
        assert_eq!(additional_recipients.len(), 1);

        let mut with_room = empty_inputs.clone();
        let show = |days_ago: i64, lag: i64| {
            let starts_at = now - time::Duration::days(days_ago);
            (starts_at, starts_at - time::Duration::days(lag))
        };
        with_room.targets.push(BookingWindowTargetInputs {
            target_id: anchor.target_id,
            room_shows: vec![show(20, 30), show(50, 30)],
            venue_coords: None,
        });
        let with_window =
            booking_candidate(booking_city(city_id), &targets, &with_room, &policy, now)?
                .ok_or_else(|| std::io::Error::other("candidate expected"))?;
        // A different proposal is a different decision — and the same action
        // identity, since the anchor and the last-outreach basis are unchanged.
        assert_ne!(with_window.decision_key, without_window.decision_key);
        assert_eq!(
            with_window.action_idempotency_key,
            without_window.action_idempotency_key
        );

        // And a changed recipient set re-decides too.
        let alone = booking_candidate(booking_city(city_id), &[anchor], &with_room, &policy, now)?
            .ok_or_else(|| std::io::Error::other("candidate expected"))?;
        assert_ne!(alone.decision_key, with_window.decision_key);
        Ok(())
    }
    /// §4e-2: an unreciprocated crossbill edge turns the partner lever into a
    /// named refusal on the decision ledger — and the refusal is that lever's,
    /// not the ladder's, so the next due lever still proposes in the same
    /// evaluation.
    #[test]
    fn an_unreciprocated_crossbill_edge_declines_the_lever_and_lets_the_ladder_move()
    -> Result<(), Box<dyn std::error::Error>> {
        use crowdrelay_domain::show_growth::{
            ShowGrowthHistory, ShowGrowthLever, ShowGrowthPolicy, ShowGrowthSnapshot,
        };

        let policy = AutopilotPolicy {
            context: AutopilotContext::ShowGrowth,
            enabled: true,
            autonomy_level: AutonomyLevel::RequireApproval,
            minimum_confidence: Confidence::from_basis_points(8_000)?,
            max_actions_24h: 10,
            config: AutopilotPolicyConfig::ShowGrowth(ShowGrowthPolicy::default()),
            version: 1,
            guarded_until: None,
            guardrail_reason: None,
        };
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let snapshot = |unreciprocated: bool| ShowGrowthSnapshot {
            event_id: EventId::new(),
            published: true,
            communication_enabled: true,
            starts_at: now + time::Duration::days(40),
            capacity: 100,
            paid_tickets: 8,
            paid_buyers: 6,
            paid_tickets_last_7d: 2,
            interested_fans: 30,
            city_signal_fans: 20,
            qualified_referrers_in_city: 4,
            beacon_partners: 0,
            attendees: 0,
            morning_after_send_at: None,
            unreciprocated_crossbill_edge: unreciprocated,
            ladder_approved: false,
            history: ShowGrowthHistory {
                canonical_link_setup_requested: true,
                free_listing_sweep_requested: true,
                audience_capture_setup_requested: true,
                ..ShowGrowthHistory::default()
            },
        };

        let candidates = show_growth::show_growth_candidates(snapshot(true), &policy, EvidenceCount(RATE_FLOOR), &std::collections::HashMap::new(), now)?;
        assert_eq!(candidates.len(), 2, "the refusal and the next due lever");
        let declined = &candidates[0];
        assert_eq!(declined.decision_kind, "unreciprocated_crossbill");
        assert_eq!(declined.disposition, PolicyDisposition::Deny);
        assert!(declined.reason.contains("unreciprocated_crossbill"));
        assert!(matches!(
            declined.action,
            AutopilotActionPayload::RequestShowGrowth {
                lever: ShowGrowthLever::PartnerCrossPromo,
                ..
            }
        ));
        // Forty days out with partner masked, grassroots scene relay is the
        // next lever the night is owed — the gate must not starve it.
        let next = &candidates[1];
        assert_eq!(next.decision_kind, "activate_show_growth_lever");
        assert!(matches!(
            next.action,
            AutopilotActionPayload::RequestShowGrowth {
                lever: ShowGrowthLever::GrassrootsSceneRelay,
                ..
            }
        ));

        // The same due lever fires the moment the edge is reciprocated —
        // one candidate, the ordinary request, no refusal row.
        let candidates = show_growth::show_growth_candidates(snapshot(false), &policy, EvidenceCount(RATE_FLOOR), &std::collections::HashMap::new(), now)?;
        assert_eq!(candidates.len(), 1);
        let proposed = &candidates[0];
        assert_eq!(proposed.decision_kind, "activate_show_growth_lever");
        assert_eq!(proposed.disposition, PolicyDisposition::RequireApproval);
        assert!(matches!(
            proposed.action,
            AutopilotActionPayload::RequestShowGrowth {
                lever: ShowGrowthLever::PartnerCrossPromo,
                ..
            }
        ));
        Ok(())
    }

    /// Generalized standing: a lever whose own measured outcomes retired it is
    /// refused on the Deny ledger — the same discipline agent templates get —
    /// while a lever with no record stays untested and fires.
    #[test]
    fn a_retired_lever_is_refused_and_an_unmeasured_one_still_fires()
    -> Result<(), Box<dyn std::error::Error>> {
        use crowdrelay_domain::learning::{RetirementReason, Standing};
        use crowdrelay_domain::show_growth::{
            ShowGrowthHistory, ShowGrowthLever, ShowGrowthPolicy, ShowGrowthSnapshot,
        };

        let policy = AutopilotPolicy {
            context: AutopilotContext::ShowGrowth,
            enabled: true,
            autonomy_level: AutonomyLevel::RequireApproval,
            minimum_confidence: Confidence::from_basis_points(8_000)?,
            max_actions_24h: 10,
            config: AutopilotPolicyConfig::ShowGrowth(ShowGrowthPolicy::default()),
            version: 1,
            guarded_until: None,
            guardrail_reason: None,
        };
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let snapshot = ShowGrowthSnapshot {
            event_id: EventId::new(),
            published: true,
            communication_enabled: true,
            starts_at: now + time::Duration::days(40),
            capacity: 100,
            paid_tickets: 8,
            paid_buyers: 6,
            paid_tickets_last_7d: 2,
            interested_fans: 30,
            city_signal_fans: 20,
            qualified_referrers_in_city: 4,
            beacon_partners: 0,
            attendees: 0,
            morning_after_send_at: None,
            unreciprocated_crossbill_edge: false,
            ladder_approved: false,
            history: ShowGrowthHistory {
                canonical_link_setup_requested: true,
                free_listing_sweep_requested: true,
                audience_capture_setup_requested: true,
                ..ShowGrowthHistory::default()
            },
        };

        let mut standings = std::collections::HashMap::new();
        standings.insert(
            "show.growth.request:partner_cross_promo".to_owned(),
            Standing::Retired {
                reason: RetirementReason::RepeatedlyWorsened,
            },
        );
        let candidates = show_growth::show_growth_candidates(
            snapshot,
            &policy,
            EvidenceCount(RATE_FLOOR),
            &standings,
            now,
        )?;
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].decision_kind, "lever_retired");
        assert_eq!(candidates[0].disposition, PolicyDisposition::Deny);
        assert!(candidates[0].reason.contains("lever_retired"));
        assert!(matches!(
            candidates[0].action,
            AutopilotActionPayload::RequestShowGrowth {
                lever: ShowGrowthLever::PartnerCrossPromo,
                ..
            }
        ));

        // An unrelated lever's retirement must not touch the one due —
        // standing keys are per lever, not per ladder.
        let mut standings = std::collections::HashMap::new();
        standings.insert(
            "show.growth.request:grassroots_scene_relay".to_owned(),
            Standing::Retired {
                reason: RetirementReason::OperatorRetired,
            },
        );
        let candidates = show_growth::show_growth_candidates(
            snapshot,
            &policy,
            EvidenceCount(RATE_FLOOR),
            &standings,
            now,
        )?;
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].decision_kind,
            "activate_show_growth_lever"
        );
        Ok(())
    }

    /// P.4: one approved ladder is the operator's yes to every rung whose own
    /// evidence gates pass — carried as `ladder_authorized` provenance the
    /// action insert honours, not as a disposition override. The class ceiling
    /// and the envelope still get their say, and `Deny` is never lifted:
    /// approving the ladder was never approving a lever the night's own facts
    /// cannot carry.
    #[test]
    fn an_approved_ladder_marks_the_rungs_it_pre_authorized()
    -> Result<(), Box<dyn std::error::Error>> {
        use crowdrelay_domain::show_growth::{
            ShowGrowthHistory, ShowGrowthLever, ShowGrowthPolicy, ShowGrowthSnapshot,
        };

        let policy = AutopilotPolicy {
            context: AutopilotContext::ShowGrowth,
            enabled: true,
            autonomy_level: AutonomyLevel::RequireApproval,
            minimum_confidence: Confidence::from_basis_points(8_000)?,
            max_actions_24h: 10,
            config: AutopilotPolicyConfig::ShowGrowth(ShowGrowthPolicy::default()),
            version: 1,
            guarded_until: None,
            guardrail_reason: None,
        };
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let snapshot = |approved: bool| ShowGrowthSnapshot {
            event_id: EventId::new(),
            published: true,
            communication_enabled: true,
            starts_at: now + time::Duration::days(40),
            capacity: 100,
            paid_tickets: 8,
            paid_buyers: 6,
            paid_tickets_last_7d: 2,
            interested_fans: 30,
            city_signal_fans: 20,
            qualified_referrers_in_city: 4,
            beacon_partners: 0,
            attendees: 0,
            morning_after_send_at: None,
            unreciprocated_crossbill_edge: false,
            ladder_approved: approved,
            history: ShowGrowthHistory {
                canonical_link_setup_requested: true,
                free_listing_sweep_requested: true,
                audience_capture_setup_requested: true,
                ..ShowGrowthHistory::default()
            },
        };

        let parked = show_growth::show_growth_candidates(snapshot(false), &policy, EvidenceCount(RATE_FLOOR), &std::collections::HashMap::new(), now)?;
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].disposition, PolicyDisposition::RequireApproval);
        assert_eq!(parked[0].policy_snapshot.get("ladder_authorized"), None);

        // The flag rides the policy snapshot — the disposition stays honest
        // about what the level and confidence computed; the class ceiling and
        // the envelope run before the action insert honours the ladder.
        let released = show_growth::show_growth_candidates(snapshot(true), &policy, EvidenceCount(RATE_FLOOR), &std::collections::HashMap::new(), now)?;
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].disposition, PolicyDisposition::RequireApproval);
        assert_eq!(
            released[0].policy_snapshot.get("ladder_authorized"),
            Some(&serde_json::Value::Bool(true))
        );
        assert!(matches!(
            released[0].action,
            AutopilotActionPayload::RequestShowGrowth {
                lever: ShowGrowthLever::PartnerCrossPromo,
                ..
            }
        ));
        Ok(())
    }

}
