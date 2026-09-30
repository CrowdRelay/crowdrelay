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
fn community_dispatches_keep_the_existing_retry_budget_per_target()
-> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let mut snapshot = fresh_video_snapshot(now);
    let target = surge_community();
    snapshot.drop_surge_failures = vec![crowdrelay_domain::content_supply::DropSurgeLaneFailure {
        lane: format!("community:{}", target.target_id.into_uuid()),
        failures: crowdrelay_domain::content_supply::DROP_SURGE_MAX_ATTEMPTS,
        last_failed_at: now,
    }];
    let candidates = content_candidates(
        &snapshot,
        &surge_policy(),
        &[target],
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

#[test]
fn a_failed_community_retries_only_itself_after_backoff() -> Result<(), Box<dyn std::error::Error>>
{
    let now = OffsetDateTime::now_utc();
    let mut snapshot = fresh_video_snapshot(now);
    let failed = surge_community();
    let untouched = surge_community();
    snapshot.drop_surge_failures = vec![crowdrelay_domain::content_supply::DropSurgeLaneFailure {
        lane: format!("community:{}", failed.target_id.into_uuid()),
        failures: 1,
        last_failed_at: now,
    }];
    let evaluate = |at| {
        content_candidates(
            &snapshot,
            &surge_policy(),
            &[failed.clone(), untouched.clone()],
            None,
            ContextEvidence::UNPROVEN,
            at,
        )
    };
    let key = |target: &crowdrelay_domain::content_supply::CommunityRelayTarget| {
        format!(
            "action:drop_surge:{}:community:{}",
            snapshot.source_id.into_uuid(),
            target.target_id.into_uuid(),
        )
    };
    let waiting = evaluate(now + time::Duration::minutes(29))?;
    assert!(
        !waiting
            .iter()
            .any(|candidate| candidate.action_idempotency_key.starts_with(&key(&failed)))
    );
    assert!(
        waiting
            .iter()
            .any(|candidate| candidate.action_idempotency_key == key(&untouched))
    );
    let due = evaluate(now + time::Duration::minutes(30))?;
    assert!(
        due.iter()
            .any(|candidate| candidate.action_idempotency_key
                == format!("{}:attempt1", key(&failed)))
    );
    assert!(
        due.iter()
            .any(|candidate| candidate.action_idempotency_key == key(&untouched))
    );
    let mut exhausted = snapshot.clone();
    exhausted.drop_surge_failures[0].failures =
        crowdrelay_domain::content_supply::DROP_SURGE_MAX_ATTEMPTS;
    let candidates = content_candidates(
        &exhausted,
        &surge_policy(),
        &[failed.clone(), untouched.clone()],
        None,
        ContextEvidence::UNPROVEN,
        now + time::Duration::hours(2),
    )?;
    assert!(
        !candidates
            .iter()
            .any(|candidate| candidate.action_idempotency_key.starts_with(&key(&failed)))
    );
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.action_idempotency_key == key(&untouched))
    );
    Ok(())
}

#[test]
fn owned_drop_retries_wait_without_blocking_other_lanes() -> Result<(), Box<dyn std::error::Error>>
{
    let now = OffsetDateTime::now_utc();
    let mut snapshot = fresh_video_snapshot(now);
    snapshot.drop_surge_failures = vec![crowdrelay_domain::content_supply::DropSurgeLaneFailure {
        lane: "telegram".to_owned(),
        failures: 2,
        last_failed_at: now,
    }];
    let evaluate = |at| {
        content_candidates(
            &snapshot,
            &surge_policy(),
            &[],
            None,
            ContextEvidence::UNPROVEN,
            at,
        )
    };
    let waiting = evaluate(now + time::Duration::minutes(59))?;
    assert!(
        !waiting
            .iter()
            .any(|candidate| candidate.action_idempotency_key.contains(":telegram"))
    );
    assert!(
        waiting
            .iter()
            .any(|candidate| candidate.action_idempotency_key.ends_with(":discord"))
    );
    let due = evaluate(now + time::Duration::minutes(60))?;
    assert!(due.iter().any(|candidate| {
        candidate
            .action_idempotency_key
            .ends_with(":telegram:attempt2")
    }));
    Ok(())
}
