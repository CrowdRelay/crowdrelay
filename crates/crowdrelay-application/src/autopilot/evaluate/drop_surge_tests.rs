// Drop-surge fan-out tests — `include!`d at the end of `mod tests` in
// `tests.rs`, so `use` items and helpers there stay in scope. Split for
// the source-size ratchet.

    /// A fresh drop as it lands in the supply — one video, fresh, linked,
    /// with every fact the surge lanes read.
    fn fresh_video_snapshot(now: OffsetDateTime) -> crowdrelay_domain::content_supply::ContentSupplySnapshot {
        use crowdrelay_domain::{
            ContentSourceId,
            content_supply::{ContentSourceKind, ContentSupplySnapshot},
        };
        ContentSupplySnapshot {
            source_id: ContentSourceId::new(),
            source_kind: ContentSourceKind::Video,
            source_version: 1,
            source_key: "youtube:iijgBMteL9I".to_owned(),
            title: "Technophobia Live From FLSS 2026".to_owned(),
            source_url: Some("https://www.youtube.com/watch?v=iijgBMteL9I".to_owned()),
            source_body: Some("Live from FLSS 2026. #staymad".to_owned()),
            source_thumbnail_url: None,
            site_origin: Some("https://virya.music".to_owned()),
            drop_surge_failures: Vec::new(),
            surge_requested_at: None,
            occurred_at: now - time::Duration::hours(1),
            expires_at: now + time::Duration::days(30),
            communication_enabled: None,
            press_enabled: None,
            release_tier: None,
            completed_artifacts: Vec::new(),
            in_flight_artifacts: Vec::new(),
            failed_artifacts: Vec::new(),
            social_post: None,
        }
    }

    fn surge_policy() -> AutopilotPolicy {
        use crowdrelay_domain::content_supply::ContentSupplyPolicy;
        AutopilotPolicy {
            context: AutopilotContext::ContentSupply,
            enabled: true,
            autonomy_level: AutonomyLevel::BoundedAuto,
            minimum_confidence: Confidence::from_basis_points(5_000)
                .expect("valid basis points"),
            max_actions_24h: 50,
            config: AutopilotPolicyConfig::ContentSupply(ContentSupplyPolicy::default()),
            version: 1,
            guarded_until: None,
            guardrail_reason: None,
        }
    }

    fn surge_community() -> crowdrelay_domain::content_supply::CommunityRelayTarget {
        use crowdrelay_domain::{
            OutreachTargetId, content_supply::CommunityRelayTarget,
        };
        CommunityRelayTarget {
            target_id: OutreachTargetId::new(),
            subreddit: "industrialmusic".to_owned(),
            language: Some("en".to_owned()),
        }
    }

    #[test]
    fn a_fresh_video_fans_out_to_every_surge_lane_at_once()
    -> Result<(), Box<dyn std::error::Error>> {
        use crowdrelay_domain::content_supply::{
            DROP_SURGE_LANES, SignalPushAudience,
        };
        let now = OffsetDateTime::now_utc();
        let snapshot = fresh_video_snapshot(now);
        let policy = surge_policy();
        let candidates = content_candidates(
            &snapshot,
            &policy,
            &[surge_community()],
            Some(SignalPushAudience { eligible: 12, reached: 12 }),
            ContextEvidence::UNPROVEN,
            now,
        )?;
        let lanes: Vec<String> = candidates
            .iter()
            .filter(|candidate| candidate.decision_kind == "drop_surge_fanout")
            .map(|candidate| {
                candidate
                    .action_idempotency_key
                    .rsplit(':')
                    .next()
                    .expect("key ends with the lane")
                    .to_owned()
            })
            .collect();
        assert_eq!(
            lanes.len(),
            DROP_SURGE_LANES.len(),
            "every lane raises in the same evaluation: {lanes:?}"
        );
        for lane in DROP_SURGE_LANES {
            assert!(lanes.iter().any(|key| key == lane), "missing lane {lane}");
        }
        // Every lane has its own subject and key — the inflight-subject
        // unique index would otherwise serialize the whole surge behind
        // whichever lane landed first.
        let subjects: std::collections::BTreeSet<String> = candidates
            .iter()
            .filter(|candidate| candidate.decision_kind == "drop_surge_fanout")
            .map(|candidate| format!("{:?}", candidate.subject))
            .collect();
        assert_eq!(subjects.len(), DROP_SURGE_LANES.len());
        let keys: std::collections::BTreeSet<&str> = candidates
            .iter()
            .filter(|candidate| candidate.decision_kind == "drop_surge_fanout")
            .map(|candidate| candidate.action_idempotency_key.as_str())
            .collect();
        assert_eq!(keys.len(), DROP_SURGE_LANES.len());
        // Bounded-auto must mean it: outward lanes auto-execute even against
        // unproven evidence, the same gate the sibling artifact lane answers.
        // The first version of this test exercised
        // `disposition_with_evidence`, and a production drop under a
        // full-send posture still parked every lane awaiting approval —
        // the evidence floor is for measured decisions, not first-party
        // posting lanes with a closing window.
        let dispositions: Vec<PolicyDisposition> = candidates
            .iter()
            .filter(|candidate| candidate.decision_kind == "drop_surge_fanout")
            .map(|candidate| candidate.disposition)
            .collect();
        let outward = dispositions
            .iter()
            .zip(
                candidates
                    .iter()
                    .filter(|c| c.decision_kind == "drop_surge_fanout")
                    .map(|c| c.action_idempotency_key.rsplit(':').next().unwrap_or("")),
            )
            .filter(|(_, lane)| *lane != "community")
            .map(|(disposition, _)| *disposition)
            .collect::<Vec<_>>();
        assert!(
            outward
                .iter()
                .all(|disposition| *disposition == PolicyDisposition::AutoExecute),
            "outward lanes auto-execute at bounded-auto: {dispositions:?}"
        );
        Ok(())
    }

    #[test]
    fn the_surge_carries_tracked_links_and_source_attribution()
    -> Result<(), Box<dyn std::error::Error>> {
        use crowdrelay_domain::content_supply::SignalPushAudience;
        let now = OffsetDateTime::now_utc();
        let snapshot = fresh_video_snapshot(now);
        let source = snapshot.source_id.into_uuid();
        let policy = surge_policy();
        let candidates = content_candidates(
            &snapshot,
            &policy,
            &[surge_community()],
            Some(SignalPushAudience { eligible: 12, reached: 12 }),
            ContextEvidence::UNPROVEN,
            now,
        )?;
        let mut saw_signal = false;
        let mut saw_email = false;
        let mut saw_community = false;
        let mut channel_lanes = 0;
        for candidate in &candidates {
            match &candidate.action {
                AutopilotActionPayload::RequestSignalPush {
                    target_path, drop_surge_lane, ..
                } => {
                    saw_signal = true;
                    assert_eq!(drop_surge_lane.as_deref(), Some("signal_push"));
                    let path = target_path.as_deref().expect("a push links somewhere");
                    assert!(path.starts_with("/l/drop-"), "tracked link, got {path}");
                }
                AutopilotActionPayload::RequestSourceCampaign {
                    source_id, draft, ..
                } => {
                    saw_email = true;
                    assert_eq!(*source_id, snapshot.source_id);
                    assert!(!draft.subject.trim().is_empty(), "email sends fail closed without a subject");
                    assert!(!draft.body.trim().is_empty());
                    assert!(draft.body.contains("https://virya.music/l/drop-"));
                }
                AutopilotActionPayload::RequestAgentRun { template_id, .. } => {
                    saw_community = true;
                    assert_eq!(template_id, "community-engager");
                }
                AutopilotActionPayload::RequestAgentContent { draft, .. } => {
                    channel_lanes += 1;
                    assert_eq!(draft["source_id"], serde_json::json!(source));
                    assert_eq!(draft["drop_surge"]["origin"], "deterministic");
                    assert!(draft["cta_url"].as_str().expect("a tracked cta").starts_with("/l/drop-"));
                    assert!(!draft["text"].as_str().expect("caption").trim().is_empty());
                }
                _ => {}
            }
        }
        assert!(saw_signal && saw_email && saw_community, "all three special lanes raised");
        assert_eq!(channel_lanes, 5, "telegram, discord, instagram, facebook, x");
        Ok(())
    }

    #[test]
    fn one_failed_surge_lane_retries_alone_and_the_rest_are_untouched()
    -> Result<(), Box<dyn std::error::Error>> {
        use crowdrelay_domain::content_supply::{
            DROP_SURGE_LANES, DropSurgeLaneFailure, SignalPushAudience,
        };
        let now = OffsetDateTime::now_utc();
        let mut snapshot = fresh_video_snapshot(now);
        snapshot.drop_surge_failures = vec![DropSurgeLaneFailure {
            lane: "telegram".to_owned(),
            failures: 1,
            last_failed_at: now - time::Duration::hours(1),
        }];
        let policy = surge_policy();
        let candidates = content_candidates(
            &snapshot,
            &policy,
            &[surge_community()],
            Some(SignalPushAudience { eligible: 12, reached: 12 }),
            ContextEvidence::UNPROVEN,
            now,
        )?;
        let surge: Vec<&DecisionCandidate> = candidates
            .iter()
            .filter(|candidate| candidate.decision_kind == "drop_surge_fanout")
            .collect();
        assert_eq!(surge.len(), DROP_SURGE_LANES.len());
        let retried = surge
            .iter()
            .filter(|candidate| candidate.action_idempotency_key.ends_with(":attempt1"))
            .count();
        assert_eq!(retried, 1, "only the failed lane carries a new key");
        Ok(())
    }

    #[test]
    fn a_surge_lane_stops_after_its_attempt_budget()
    -> Result<(), Box<dyn std::error::Error>> {
        use crowdrelay_domain::content_supply::{
            DROP_SURGE_LANES, DROP_SURGE_MAX_ATTEMPTS, DropSurgeLaneFailure, SignalPushAudience,
        };
        let now = OffsetDateTime::now_utc();
        let mut snapshot = fresh_video_snapshot(now);
        snapshot.drop_surge_failures = vec![DropSurgeLaneFailure {
            lane: "x".to_owned(),
            failures: DROP_SURGE_MAX_ATTEMPTS,
            last_failed_at: now - time::Duration::minutes(5),
        }];
        let policy = surge_policy();
        let candidates = content_candidates(
            &snapshot,
            &policy,
            &[surge_community()],
            Some(SignalPushAudience { eligible: 12, reached: 12 }),
            ContextEvidence::UNPROVEN,
            now,
        )?;
        let surge: Vec<&DecisionCandidate> = candidates
            .iter()
            .filter(|candidate| candidate.decision_kind == "drop_surge_fanout")
            .collect();
        assert_eq!(surge.len(), DROP_SURGE_LANES.len() - 1, "x is done; the other lanes still run");
        assert!(!surge.iter().any(|candidate| {
            candidate.action_idempotency_key.ends_with(":x")
                || candidate.action_idempotency_key.contains(":x:")
        }));
        Ok(())
    }

    #[test]
    fn a_stale_video_does_not_surge_until_an_operator_promotes_it()
    -> Result<(), Box<dyn std::error::Error>> {
        use crowdrelay_domain::content_supply::SignalPushAudience;
        let now = OffsetDateTime::now_utc();
        let mut snapshot = fresh_video_snapshot(now);
        snapshot.occurred_at = now - time::Duration::days(9);
        let policy = surge_policy();
        let stale = content_candidates(
            &snapshot,
            &policy,
            &[surge_community()],
            Some(SignalPushAudience { eligible: 12, reached: 12 }),
            ContextEvidence::UNPROVEN,
            now,
        )?;
        assert!(
            !stale.iter().any(|candidate| candidate.decision_kind == "drop_surge_fanout"),
            "a nine-day-old video stays on the ordinary cadence"
        );
        // The operator's promote re-arms the same source for 24 hours.
        snapshot.surge_requested_at = Some(now - time::Duration::minutes(3));
        let promoted = content_candidates(
            &snapshot,
            &policy,
            &[surge_community()],
            Some(SignalPushAudience { eligible: 12, reached: 12 }),
            ContextEvidence::UNPROVEN,
            now,
        )?;
        assert!(
            promoted.iter().any(|candidate| candidate.decision_kind == "drop_surge_fanout"),
            "an explicit promote re-opens the drop window"
        );
        Ok(())
    }

    #[test]
    fn a_source_without_a_link_or_with_communication_off_never_surges()
    -> Result<(), Box<dyn std::error::Error>> {
        let now = OffsetDateTime::now_utc();
        let policy = surge_policy();
        for (url, communication) in [
            (None, None),
            (Some("ftp://not-a-link"), None),
            (Some("https://youtu.be/x"), Some(false)),
        ] {
            let mut snapshot = fresh_video_snapshot(now);
            snapshot.source_url = url.map(str::to_owned);
            snapshot.communication_enabled = communication;
            let candidates = content_candidates(
                &snapshot, &policy, &[surge_community()], None,
                ContextEvidence::UNPROVEN, now,
            )?;
            assert!(
                !candidates.iter().any(|candidate| candidate.decision_kind == "drop_surge_fanout"),
                "url={url:?} communication={communication:?} must not surge"
            );
        }
        Ok(())
    }
