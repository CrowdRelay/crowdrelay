#[test]
fn excluded_video_platforms_cannot_produce_owned_or_community_candidates()
-> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let mut snapshot = fresh_video_snapshot(now);
    snapshot.promotion_excluded_platforms = vec!["facebook", "instagram", "reddit", "signal_push"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let candidates = content_candidates(
        &snapshot,
        &surge_policy(),
        &[surge_community()],
        None,
        ContextEvidence::UNPROVEN,
        now,
    )?;
    assert!(!candidates.iter().any(|candidate| matches!(
        candidate.action,
        AutopilotActionPayload::RequestSignalPush { .. }
            | AutopilotActionPayload::RequestAgentRun { .. }
    )));
    for candidate in candidates {
        if let AutopilotActionPayload::RequestAgentContent { draft, .. } = candidate.action {
            assert_ne!(draft["platform"], "facebook");
            assert_ne!(draft["platform"], "instagram");
        }
    }
    Ok(())
}

#[test]
fn a_video_dispatch_pins_each_forum_and_discord_identity_without_reddit_routing()
-> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let snapshot = fresh_video_snapshot(now);
    let mut forum = surge_community();
    forum.platform = "forum".to_owned();
    forum.community_url = Some("https://forum.example/music".to_owned());
    let mut discord = surge_community();
    discord.platform = "discord".to_owned();
    discord.community_url = Some("https://discord.com/channels/123/456".to_owned());
    let candidates = content_candidates(
        &snapshot,
        &surge_policy(),
        &[forum.clone(), discord.clone()],
        None,
        ContextEvidence::UNPROVEN,
        now,
    )?;
    let drafts: Vec<_> = candidates
        .iter()
        .filter(|candidate| {
            matches!(
                candidate.action,
                AutopilotActionPayload::RequestAgentRun { .. }
            )
        })
        .collect();
    assert_eq!(drafts.len(), 2);
    assert_ne!(
        drafts[0].action_idempotency_key,
        drafts[1].action_idempotency_key
    );
    for (candidate, target) in drafts.iter().zip([forum, discord]) {
        let AutopilotActionPayload::RequestAgentRun { prompt, .. } = &candidate.action else {
            unreachable!()
        };
        assert!(prompt.contains(&format!("source_id: {}", snapshot.source_id.into_uuid())));
        assert!(prompt.contains(&format!("target_id: {}", target.target_id.into_uuid())));
        assert!(prompt.contains(&format!("platform: {}", target.platform)));
        assert!(prompt.contains(target.community_url.as_deref().expect("destination")));
        assert!(!prompt.contains("r/"));
        assert!(prompt.contains("not permission to publish"));
    }
    Ok(())
}

#[test]
fn community_dispatches_keep_the_existing_retry_budget() -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let mut snapshot = fresh_video_snapshot(now);
    snapshot.drop_surge_failures = vec![crowdrelay_domain::content_supply::DropSurgeLaneFailure {
        lane: "community".to_owned(),
        failures: crowdrelay_domain::content_supply::DROP_SURGE_MAX_ATTEMPTS,
        last_failed_at: now,
    }];
    let candidates = content_candidates(
        &snapshot,
        &surge_policy(),
        &[surge_community()],
        None,
        ContextEvidence::UNPROVEN,
        now,
    )?;
    assert!(!candidates.iter().any(|candidate| matches!(
        candidate.action,
        AutopilotActionPayload::RequestAgentRun { .. }
    )));
    Ok(())
}
