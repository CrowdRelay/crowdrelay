// Drop-surge fan-out tests — `include!`d at the end of `mod tests` in
// `tests.rs`, so `use` items and helpers there stay in scope. Split for
// the source-size ratchet.

/// A fresh drop as it lands in the supply — one video, fresh, linked,
/// with every fact the surge lanes read.
fn fresh_video_snapshot(
    now: OffsetDateTime,
) -> crowdrelay_domain::content_supply::ContentSupplySnapshot {
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
        promotion_excluded_platforms: Vec::new(),
        occurred_at: now - time::Duration::hours(1),
        expires_at: now + time::Duration::days(30),
        communication_enabled: None,
        press_enabled: None,
        release_tier: None,
        completed_artifacts: Vec::new(),
        in_flight_artifacts: Vec::new(),
        failed_artifacts: Vec::new(),
        lane_outages: Vec::new(),
        social_post: None,
    }
}

fn fresh_release_snapshot(
    now: OffsetDateTime,
) -> crowdrelay_domain::content_supply::ContentSupplySnapshot {
    let mut snapshot = fresh_video_snapshot(now);
    snapshot.source_kind = crowdrelay_domain::content_supply::ContentSourceKind::Release;
    snapshot.source_key = format!("release:{}", snapshot.source_id.into_uuid());
    snapshot.title = "New album".to_owned();
    snapshot.source_url = Some("https://open.spotify.com/album/test-release".to_owned());
    snapshot.source_body = Some("Our new album is out now.".to_owned());
    snapshot.source_thumbnail_url = None;
    snapshot.communication_enabled = Some(true);
    snapshot
}

fn surge_policy() -> AutopilotPolicy {
    use crowdrelay_domain::content_supply::ContentSupplyPolicy;
    AutopilotPolicy {
        context: AutopilotContext::ContentSupply,
        enabled: true,
        autonomy_level: AutonomyLevel::BoundedAuto,
        minimum_confidence: Confidence::from_basis_points(5_000).expect("valid basis points"),
        max_actions_24h: 50,
        config: AutopilotPolicyConfig::ContentSupply(ContentSupplyPolicy::default()),
        version: 1,
        guarded_until: None,
        guardrail_reason: None,
    }
}

/// A room the band has read: three threads from the last few days, each with
/// its own permalink. The drop surge posts only into a room like this one.
pub(super) fn read_room() -> Vec<crowdrelay_domain::room_reading::RoomThread> {
    let today = OffsetDateTime::now_utc().date();
    (1..=3)
        .map(|n| crowdrelay_domain::room_reading::RoomThread {
            title: format!("What is everyone listening to, week {n}"),
            url: format!("https://www.reddit.com/comments/room{n}"),
            posted_on: today - time::Duration::days(i64::from(n)),
        })
        .collect()
}

fn surge_community() -> crowdrelay_domain::content_supply::CommunityRelayTarget {
    use crowdrelay_domain::{OutreachTargetId, content_supply::CommunityRelayTarget};
    CommunityRelayTarget {
        target_id: OutreachTargetId::new(),
        subreddit: "industrialmusic".to_owned(),
        platform: "reddit".to_owned(),
        community_url: None,
        language: Some("en".to_owned()),
        relay_failures: Vec::new(),
 recent_threads: read_room(),
    }
}

#[test]
fn a_fresh_video_fans_out_to_every_surge_lane_at_once() -> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_domain::content_supply::{DROP_SURGE_LANES, SignalPushAudience};
    let now = OffsetDateTime::now_utc();
    let snapshot = fresh_video_snapshot(now);
    let policy = surge_policy();
    let candidates = content_candidates(
        &snapshot,
        &policy,
        &[surge_community()],
        Some(SignalPushAudience {
            eligible: 12,
            reached: 12,
        }),
        ContextEvidence::UNPROVEN,
        now,
    )?;
    let lanes: Vec<String> = candidates
        .iter()
        .filter(|candidate| candidate.decision_kind == "drop_surge_fanout")
        .map(|candidate| {
            candidate
                .action_idempotency_key
                .split(':')
                .nth(3)
                .expect("key carries the lane")
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
                .map(|c| c.action_idempotency_key.split(':').nth(3).unwrap_or("")),
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
fn a_fresh_release_reaches_read_admitted_communities_too()
-> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_domain::content_supply::{DROP_SURGE_LANES, SignalPushAudience};
    let now = OffsetDateTime::now_utc();
    let snapshot = fresh_release_snapshot(now);
    let source = snapshot.source_id.into_uuid();
    let community = surge_community();
    let target = community.target_id.into_uuid();
    let candidates = content_candidates(
        &snapshot,
        &surge_policy(),
        &[community],
        Some(SignalPushAudience {
            eligible: 12,
            reached: 12,
        }),
        ContextEvidence::UNPROVEN,
        now,
    )?;

    let surge: Vec<&DecisionCandidate> = candidates
        .iter()
        .filter(|candidate| candidate.decision_kind == "drop_surge_fanout")
        .collect();
    assert_eq!(
        surge.len(),
        DROP_SURGE_LANES.len(),
        "a linked fresh release owes the same first-day fan-out as a video"
    );
    let community = surge
        .iter()
        .find(|candidate| {
            matches!(
                &candidate.action,
                AutopilotActionPayload::RequestAgentRun { template_id, .. }
                    if template_id == "community-engager"
            )
        })
        .expect("release gets a community-engager dispatch");
    assert_eq!(community.subject, ActionSubject::TargetCommunity(target));
    let AutopilotActionPayload::RequestAgentRun { prompt, .. } = &community.action else {
        unreachable!("matched above")
    };
    assert!(prompt.contains(&format!("source_id: {source}")));
    assert!(prompt.contains("https://open.spotify.com/album/test-release"));
    Ok(())
}

#[test]
fn the_surge_carries_tracked_links_and_source_attribution() -> Result<(), Box<dyn std::error::Error>>
{
    use crowdrelay_domain::content_supply::SignalPushAudience;
    let now = OffsetDateTime::now_utc();
    let snapshot = fresh_video_snapshot(now);
    let source = snapshot.source_id.into_uuid();
    let policy = surge_policy();
    let candidates = content_candidates(
        &snapshot,
        &policy,
        &[surge_community()],
        Some(SignalPushAudience {
            eligible: 12,
            reached: 12,
        }),
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
                target_path,
                drop_surge_lane,
                ..
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
                assert!(
                    !draft.subject.trim().is_empty(),
                    "email sends fail closed without a subject"
                );
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
                assert!(
                    draft["cta_url"]
                        .as_str()
                        .expect("a tracked cta")
                        .starts_with("/l/drop-")
                );
                assert!(!draft["text"].as_str().expect("caption").trim().is_empty());
            }
            _ => {}
        }
    }
    assert!(
        saw_signal && saw_email && saw_community,
        "all three special lanes raised"
    );
    assert_eq!(
        channel_lanes, 5,
        "telegram, discord, instagram, facebook, x"
    );
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
        Some(SignalPushAudience {
            eligible: 12,
            reached: 12,
        }),
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
fn a_surge_lane_stops_after_its_attempt_budget() -> Result<(), Box<dyn std::error::Error>> {
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
        Some(SignalPushAudience {
            eligible: 12,
            reached: 12,
        }),
        ContextEvidence::UNPROVEN,
        now,
    )?;
    let surge: Vec<&DecisionCandidate> = candidates
        .iter()
        .filter(|candidate| candidate.decision_kind == "drop_surge_fanout")
        .collect();
    assert_eq!(
        surge.len(),
        DROP_SURGE_LANES.len() - 1,
        "x is done; the other lanes still run"
    );
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
        Some(SignalPushAudience {
            eligible: 12,
            reached: 12,
        }),
        ContextEvidence::UNPROVEN,
        now,
    )?;
    assert!(
        !stale
            .iter()
            .any(|candidate| candidate.decision_kind == "drop_surge_fanout"),
        "a nine-day-old video stays on the ordinary cadence"
    );
    // The operator's promote re-arms the same source for 24 hours.
    snapshot.surge_requested_at = Some(now - time::Duration::minutes(3));
    let promoted = content_candidates(
        &snapshot,
        &policy,
        &[surge_community()],
        Some(SignalPushAudience {
            eligible: 12,
            reached: 12,
        }),
        ContextEvidence::UNPROVEN,
        now,
    )?;
    assert!(
        promoted
            .iter()
            .any(|candidate| candidate.decision_kind == "drop_surge_fanout"),
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
            &snapshot,
            &policy,
            &[surge_community()],
            None,
            ContextEvidence::UNPROVEN,
            now,
        )?;
        assert!(
            !candidates
                .iter()
                .any(|candidate| candidate.decision_kind == "drop_surge_fanout"),
            "url={url:?} communication={communication:?} must not surge"
        );
    }
    Ok(())
}

/// A relay dispatch whose action — or the drafting task it queued —
/// failed is neither sent nor in flight. Re-emitting it under the same
/// key dedupes onto the dead action while the target loader keeps
/// re-selecting the community, so the retry carries its attempt number
/// and waits out the same backoff the artifact chain uses.
#[test]
fn a_failed_community_relay_retries_under_an_attempt_key()
-> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_domain::{
        ContentSourceId, OutreachTargetId,
        content_supply::{
            CommunityRelayTarget, ContentSupplyPolicy, ContentSupplySnapshot, PostResonance,
            RelayLaneFailure, SocialPostFact,
        },
    };
    let now = OffsetDateTime::now_utc();
    let source = ContentSourceId::new();
    let snapshot = ContentSupplySnapshot {
        source_id: source,
        source_kind: ContentSourceKind::SocialPost,
        source_version: 1,
        source_key: String::new(),
        title: String::new(),
        source_url: None,
        source_body: None,
        source_thumbnail_url: None,
        site_origin: None,
        drop_surge_failures: Vec::new(),
        surge_requested_at: None,
        promotion_excluded_platforms: Vec::new(),
        occurred_at: now - time::Duration::hours(40),
        expires_at: now + time::Duration::days(40),
        communication_enabled: None,
        press_enabled: None,
        release_tier: None,
        completed_artifacts: Vec::new(),
        in_flight_artifacts: Vec::new(),
        failed_artifacts: Vec::new(),
        lane_outages: Vec::new(),
        social_post: Some(SocialPostFact {
            title: "rehearsal cut".to_owned(),
            url: Some("https://instagram.com/p/xyz".to_owned()),
            platform: "instagram".to_owned(),
            body: Some("rehearsal cut of the new one".to_owned()),
            media_url: None,
            media_id: None,
            media_type: None,
            thumbnail_url: None,
            acquired_fans: 1,
            resonance: Some(PostResonance {
                engagement: 10,
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
    let community = |failures: Vec<RelayLaneFailure>| {
        CommunityRelayTarget {
            target_id: OutreachTargetId::new(),
            subreddit: "doommetal".to_owned(),
            platform: "reddit".to_owned(),
            community_url: None,
            language: Some("en".to_owned()),
            relay_failures: failures,
 recent_threads: read_room(),
        }
    };
    let failure = |failures: u32, ago: time::Duration| RelayLaneFailure {
        source_id: source,
        failures,
        last_failed_at: now - ago,
    };
    let relay_key = |communities: &[CommunityRelayTarget]| {
        content_candidates(&snapshot, &policy, communities, None, ContextEvidence::UNPROVEN, now)
            .map(|candidates| {
                candidates.iter().find_map(|candidate| {
                    candidate
                        .action_idempotency_key
                        .contains(":community:")
                        .then(|| candidate.action_idempotency_key.clone())
                })
            })
    };

    // A lane with no failures emits the base key; one due failure emits
    // the first retry key — a new action, not a dedupe onto the dead one.
    let fresh = relay_key(&[community(Vec::new())])?;
    assert!(fresh.is_some_and(|key| !key.contains(":attempt")));
    let due = relay_key(&[community(vec![failure(1, time::Duration::minutes(31))])])?;
    assert_eq!(due.as_deref().map(|key| &key[key.len() - 8..]), Some("attempt1"));

    // Inside the backoff window the lane waits; at the attempt cap it
    // stays silent rather than spamming the community.
    let waiting = relay_key(&[community(vec![failure(1, time::Duration::minutes(5))])])?;
    assert!(waiting.is_none(), "a lane in backoff emits nothing");
    let spent = relay_key(&[community(vec![failure(
        crowdrelay_domain::content_supply::MAX_ARTIFACT_ATTEMPTS,
        time::Duration::days(30),
    )])])?;
    assert!(spent.is_none(), "an exhausted lane emits nothing");

    // A failure that carried another source never touches this lane.
    let foreign = relay_key(&[community(vec![RelayLaneFailure {
        source_id: ContentSourceId::new(),
        failures: crowdrelay_domain::content_supply::MAX_ARTIFACT_ATTEMPTS,
        last_failed_at: now - time::Duration::days(30),
    }])])?;
    assert!(foreign.is_some_and(|key| !key.contains(":attempt")));
    Ok(())
}

fn community_drafts(
    communities: &[crowdrelay_domain::content_supply::CommunityRelayTarget],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let candidates = content_candidates(
        &fresh_video_snapshot(now),
        &surge_policy(),
        communities,
        None,
        ContextEvidence::UNPROVEN,
        now,
    )?;
    Ok(candidates
        .iter()
        .filter_map(|candidate| match &candidate.action {
            AutopilotActionPayload::RequestAgentRun { prompt, .. } => Some(prompt.clone()),
            _ => None,
        })
        .collect())
}

/// A fresh drop is time-sensitive, and still not worth a post into a room nobody
/// looked into: the community lane waits on the band having read the room.
#[test]
fn the_drop_surge_does_not_post_into_a_room_the_band_has_not_read() -> Result<(), Box<dyn std::error::Error>>
{
    let read = surge_community();
    let mut unread = surge_community();
    unread.subreddit = "quietplace".to_owned();
    unread.recent_threads.truncate(2);
    let mut forum = surge_community();
    forum.platform = "forum".to_owned();
    forum.community_url = Some("https://forum.example/music".to_owned());
    forum.recent_threads.clear();

    let drafts = community_drafts(&[read.clone(), unread, forum])?;
    assert_eq!(drafts.len(), 1, "only the read room is drafted for: {drafts:?}");
    assert!(drafts[0].contains("r/industrialmusic"));
    // The draft is told what the room is discussing, with the URLs it may cite.
    for thread in &read.recent_threads {
        assert!(drafts[0].contains(&thread.url), "{}", drafts[0]);
    }
    assert!(drafts[0].contains("fits_thread_url"));
    Ok(())
}
