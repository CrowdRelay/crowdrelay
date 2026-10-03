
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
            promotion_excluded_platforms: Vec::new(),
            source_id: ContentSourceId::new(),
            source_kind: ContentSourceKind::SocialPost,
            source_version: 3,
            source_key: String::new(),
            title: String::new(),
            source_url: None,
            source_body: None,
            source_thumbnail_url: None,
            site_origin: None,
            drop_surge_failures: Vec::new(),
            surge_requested_at: None,
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
            lane_outages: Vec::new(),
            social_post: Some(SocialPostFact {
                title: "soundcheck done".to_owned(),
                url: Some("https://instagram.com/p/abc".to_owned()),
                platform: "instagram".to_owned(),
                body: Some("soundcheck done — see you tonight".to_owned()),
                media_url: Some("https://cdn.instagram.example/img.jpg".to_owned()),
                media_id: Some("1790".to_owned()),
                media_type: Some("IMAGE".to_owned()),
                thumbnail_url: None,
                acquired_fans: 0,
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
                platform: "reddit".to_owned(),
                community_url: None,
                language: Some("en".to_owned()),
                relay_failures: Vec::new(),
 recent_threads: Vec::new(),
            },
            CommunityRelayTarget {
                target_id: OutreachTargetId::new(),
                subreddit: "listentothis".to_owned(),
                platform: "reddit".to_owned(),
                community_url: None,
                language: None,
                relay_failures: Vec::new(),
 recent_threads: Vec::new(),
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
            candidates.iter().all(|candidate| !matches!(
                candidate.action,
                AutopilotActionPayload::RequestSignalPush { .. }
            )),
            "no push for a passed tonight"
        );
        assert_eq!(candidates.len(), 2, "the two community drafts remain");
        Ok(())
    }
