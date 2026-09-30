#[cfg(test)]
mod tests_booking {
    use super::*;
    use crowdrelay_domain::{
        BookingTargetId, CityId, EventId,
        autonomy::{AutonomyLevel, Confidence, EvidenceCount, PolicyDisposition},
    };

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
    /// With no external executor live, the candidates skip the levers only
    /// it could carry out, and the first-party lever behind them is
    /// proposed. With one live, the ladder proposes the external lever as
    /// before.
    #[test]
    fn the_ladder_passes_over_levers_nothing_can_run() -> Result<(), Box<dyn std::error::Error>> {
        use crowdrelay_domain::show_growth::{
            ShowGrowthHistory, ShowGrowthLever, ShowGrowthPolicy, ShowGrowthSnapshot,
        };

        let policy = AutopilotPolicy {
            context: AutopilotContext::ShowGrowth,
            enabled: true,
            autonomy_level: AutonomyLevel::BoundedAuto,
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
            // Inside the free fan-channel push window (18 days), behind
            // every external lever before it.
            starts_at: now + time::Duration::days(17),
            capacity: 100,
            paid_tickets: 0,
            paid_buyers: 0,
            paid_tickets_last_7d: 0,
            interested_fans: 1,
            city_signal_fans: 2,
            qualified_referrers_in_city: 0,
            beacon_partners: 0,
            human_booking_targets_30d: 0,
            attendees: 0,
            morning_after_send_at: None,
            unreciprocated_crossbill_edge: false,
            ladder_approved: false,
            history: ShowGrowthHistory {
                canonical_link_setup_requested: true,
                ..ShowGrowthHistory::default()
            },
        };
        let lever = |live: bool| -> Result<ShowGrowthLever, Box<dyn std::error::Error>> {
            let candidates = show_growth::show_growth_candidates(
                snapshot,
                &policy,
                ContextEvidence::measured(EvidenceCount(RATE_FLOOR)),
                &std::collections::HashMap::new(),
                live,
                &std::collections::HashMap::new(),
                now,
            )?;
            match candidates.first().map(|candidate| &candidate.action) {
                Some(AutopilotActionPayload::RequestShowGrowth { lever, .. }) => Ok(*lever),
                other => Err(format!("no lever proposed: {other:?}").into()),
            }
        };
        assert_eq!(lever(true)?, ShowGrowthLever::FreeListingSweep);
        assert_eq!(lever(false)?, ShowGrowthLever::FreeFanChannelPush);
        Ok(())
    }

    /// A failed lever is retried under its own key, then passed over: one
    /// bad attempt at the tracked link (Gorzów, 2026-08-23) had frozen the
    /// whole ladder for that show, because the failed row still held the key.
    #[test]
    fn a_failed_lever_is_retried_then_passed_over() -> Result<(), Box<dyn std::error::Error>> {
        use crowdrelay_domain::show_growth::{
            MAX_LEVER_ATTEMPTS, ShowGrowthHistory, ShowGrowthLever, ShowGrowthPolicy,
            ShowGrowthSnapshot,
        };

        let policy = AutopilotPolicy {
            context: AutopilotContext::ShowGrowth,
            enabled: true,
            autonomy_level: AutonomyLevel::BoundedAuto,
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
            starts_at: now + time::Duration::days(17),
            capacity: 100,
            paid_tickets: 0,
            paid_buyers: 0,
            paid_tickets_last_7d: 0,
            interested_fans: 1,
            city_signal_fans: 2,
            qualified_referrers_in_city: 0,
            beacon_partners: 0,
            human_booking_targets_30d: 0,
            attendees: 0,
            morning_after_send_at: None,
            unreciprocated_crossbill_edge: false,
            ladder_approved: false,
            history: ShowGrowthHistory::default(),
        };
        let first = |failed: u32| -> Result<DecisionCandidate, Box<dyn std::error::Error>> {
            let mut failures = std::collections::HashMap::new();
            failures.insert(
                (snapshot.event_id.into_uuid(), "canonical_link_setup".to_owned()),
                failed,
            );
            show_growth::show_growth_candidates(
                snapshot,
                &policy,
                ContextEvidence::measured(EvidenceCount(RATE_FLOOR)),
                &std::collections::HashMap::new(),
                false,
                &failures,
                now,
            )?
            .into_iter()
            .next()
            .ok_or_else(|| "a lever is proposed".into())
        };

        let retry = first(1)?;
        assert!(matches!(
            retry.action,
            AutopilotActionPayload::RequestShowGrowth {
                lever: ShowGrowthLever::CanonicalLinkSetup,
                ..
            }
        ));
        assert!(
            retry.action_idempotency_key.ends_with(":canonical_link_setup:retry1"),
            "{}",
            retry.action_idempotency_key
        );

        let past = first(MAX_LEVER_ATTEMPTS)?;
        assert!(
            matches!(
                past.action,
                AutopilotActionPayload::RequestShowGrowth {
                    lever: ShowGrowthLever::FreeFanChannelPush,
                    ..
                }
            ),
            "after the last attempt the ladder moves on: {:?}",
            past.action
        );
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
            human_booking_targets_30d: 0,
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

        let candidates = show_growth::show_growth_candidates(snapshot(true), &policy, ContextEvidence::measured(EvidenceCount(RATE_FLOOR)), &std::collections::HashMap::new(), true, &std::collections::HashMap::new(), now)?;
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
        let candidates = show_growth::show_growth_candidates(snapshot(false), &policy, ContextEvidence::measured(EvidenceCount(RATE_FLOOR)), &std::collections::HashMap::new(), true, &std::collections::HashMap::new(), now)?;
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
            human_booking_targets_30d: 0,
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
            ContextEvidence::measured(EvidenceCount(RATE_FLOOR)),
            &standings,
            true,
            &std::collections::HashMap::new(),
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
            ContextEvidence::measured(EvidenceCount(RATE_FLOOR)),
            &standings,
            true,
            &std::collections::HashMap::new(),
            now,
        )?;
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].decision_kind,
            "activate_show_growth_lever"
        );
        Ok(())
    }

    /// Adaptive autonomy: when the tenant is visibly booking, partner outreach
    /// stays per-action and human-led. When that activity is sparse, the same
    /// explicit show-ladder approval becomes the bounded backstop.
    #[test]
    fn show_ladder_partner_autonomy_tracks_tenant_booking_activity()
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
        let snapshot = |approved: bool, human_booking_targets_30d: u32| ShowGrowthSnapshot {
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
            human_booking_targets_30d,
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

        let parked = show_growth::show_growth_candidates(snapshot(false, 0), &policy, ContextEvidence::measured(EvidenceCount(RATE_FLOOR)), &std::collections::HashMap::new(), true, &std::collections::HashMap::new(), now)?;
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].disposition, PolicyDisposition::RequireApproval);
        assert_eq!(parked[0].policy_snapshot.get("ladder_authorized"), None);

        // Three distinct tenant-side targets in 30 days means there is a real
        // booking cadence to work with: CrowdRelay assists but does not consume
        // the relationship on a broad campaign approval.
        let active = show_growth::show_growth_candidates(snapshot(true, 3), &policy, ContextEvidence::measured(EvidenceCount(RATE_FLOOR)), &std::collections::HashMap::new(), true, &std::collections::HashMap::new(), now)?;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].disposition, PolicyDisposition::RequireApproval);
        assert_eq!(active[0].policy_snapshot.get("ladder_authorized"), None);

        // Sparse booking activity changes the evidence, not the authority.
        // The same partner move remains individually reviewable.
        let quiet = show_growth::show_growth_candidates(snapshot(true, 1), &policy, ContextEvidence::measured(EvidenceCount(RATE_FLOOR)), &std::collections::HashMap::new(), true, &std::collections::HashMap::new(), now)?;
        assert_eq!(quiet.len(), 1);
        assert_eq!(quiet[0].disposition, PolicyDisposition::RequireApproval);
        assert_eq!(quiet[0].policy_snapshot.get("ladder_authorized"), None);
        assert_eq!(
            quiet[0].policy_snapshot.get("relationship_backstop_authorized"),
            None
        );
        assert!(matches!(
            quiet[0].action,
            AutopilotActionPayload::RequestShowGrowth {
                lever: ShowGrowthLever::PartnerCrossPromo,
                ..
            }
        ));
        Ok(())
    }
}
