//! Split out of `content_supply.rs` for the source-size ratchet — the
//! supply-chain unit tests, unchanged. `use super::*` resolves to the
//! parent module, so every name these tests touch stays in scope.

use super::*;

fn now() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
}

#[test]
fn event_requests_live_listing_before_channel_specific_artifacts() {
    let snapshot = ContentSupplySnapshot {
        promotion_excluded_platforms: Vec::new(),
        source_id: ContentSourceId::new(),
        source_kind: ContentSourceKind::Event,
        source_version: 1,
        occurred_at: now() - Duration::days(1),
        expires_at: now() + Duration::days(10),
        communication_enabled: None,
        press_enabled: None,
        release_tier: None,
        completed_artifacts: Vec::new(),
        in_flight_artifacts: Vec::new(),
        failed_artifacts: Vec::new(),
        social_post: None,
        source_key: String::new(),
        title: String::new(),
        source_url: None,
        source_body: None,
        source_thumbnail_url: None,
        site_origin: None,
        drop_surge_failures: Vec::new(),
        surge_requested_at: None,
    };

    assert_eq!(
        evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
        ContentSupplyDecision::Request {
            artifact: ContentArtifactKind::LiveListing,
            attempt: 0,
            confidence: Confidence::saturating_from_basis_points(9_500),
        }
    );
}

#[test]
fn event_builds_press_hook_after_canonical_listing() {
    let snapshot = ContentSupplySnapshot {
        promotion_excluded_platforms: Vec::new(),
        source_id: ContentSourceId::new(),
        source_kind: ContentSourceKind::Event,
        source_version: 1,
        occurred_at: now() - Duration::days(1),
        expires_at: now() + Duration::days(10),
        communication_enabled: None,
        press_enabled: None,
        release_tier: None,
        social_post: None,
        source_key: String::new(),
        title: String::new(),
        source_url: None,
        source_body: None,
        source_thumbnail_url: None,
        site_origin: None,
        drop_surge_failures: Vec::new(),
        surge_requested_at: None,
        completed_artifacts: vec![ContentArtifactKind::LiveListing],
        in_flight_artifacts: Vec::new(),
        failed_artifacts: Vec::new(),
    };

    assert_eq!(
        evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
        ContentSupplyDecision::Request {
            artifact: ContentArtifactKind::PressHook,
            attempt: 0,
            confidence: Confidence::saturating_from_basis_points(9_500),
        }
    );
}

fn event_with_failed_listing(failures: u32, minutes_ago: i64) -> ContentSupplySnapshot {
    ContentSupplySnapshot {
        promotion_excluded_platforms: Vec::new(),
        source_id: ContentSourceId::new(),
        source_kind: ContentSourceKind::Event,
        source_version: 1,
        occurred_at: now() - Duration::days(1),
        expires_at: now() + Duration::days(10),
        communication_enabled: None,
        press_enabled: None,
        release_tier: None,
        social_post: None,
        source_key: String::new(),
        title: String::new(),
        source_url: None,
        source_body: None,
        source_thumbnail_url: None,
        site_origin: None,
        drop_surge_failures: Vec::new(),
        surge_requested_at: None,
        completed_artifacts: Vec::new(),
        in_flight_artifacts: Vec::new(),
        failed_artifacts: vec![FailedArtifact {
            artifact: ContentArtifactKind::LiveListing,
            failures,
            last_failed_at: now() - Duration::minutes(minutes_ago),
        }],
    }
}

fn requested(snapshot: &ContentSupplySnapshot) -> Option<(ContentArtifactKind, u32)> {
    match evaluate_content_supply(snapshot, ContentSupplyPolicy::default(), now()) {
        ContentSupplyDecision::Request {
            artifact, attempt, ..
        } => Some((artifact, attempt)),
        _ => None,
    }
}

#[test]
fn a_failed_artifact_is_retried_once_due_under_a_new_attempt() {
    // Production, 2026-09-25: a live listing refused with HTTP 429 was
    // asked for again under its old key every cycle, deduped onto the
    // failed action, and the source never got another artifact.
    assert_eq!(
        requested(&event_with_failed_listing(1, 31)),
        Some((ContentArtifactKind::LiveListing, 1))
    );
    assert_eq!(
        requested(&event_with_failed_listing(2, 61)),
        Some((ContentArtifactKind::LiveListing, 2))
    );
}

#[test]
fn a_retry_not_yet_due_does_not_hold_the_chain() {
    assert_eq!(
        requested(&event_with_failed_listing(1, 10)),
        Some((ContentArtifactKind::PressHook, 0))
    );
    // The second retry waits an hour, not thirty minutes.
    assert_eq!(
        requested(&event_with_failed_listing(2, 45)),
        Some((ContentArtifactKind::PressHook, 0))
    );
}

#[test]
fn an_artifact_that_failed_every_attempt_is_skipped() {
    assert_eq!(
        requested(&event_with_failed_listing(MAX_ARTIFACT_ATTEMPTS, 10_000)),
        Some((ContentArtifactKind::PressHook, 0))
    );
}

#[test]
fn release_requests_artifacts_one_at_a_time_and_respects_inflight() {
    let snapshot = ContentSupplySnapshot {
        promotion_excluded_platforms: Vec::new(),
        source_id: ContentSourceId::new(),
        source_kind: ContentSourceKind::Release,
        source_version: 1,
        occurred_at: now() - Duration::days(1),
        expires_at: now() + Duration::days(10),
        communication_enabled: Some(true),
        press_enabled: Some(true),
        release_tier: Some(ReleaseTier::Single),
        social_post: None,
        source_key: String::new(),
        title: String::new(),
        source_url: None,
        source_body: None,
        source_thumbnail_url: None,
        site_origin: None,
        drop_surge_failures: Vec::new(),
        surge_requested_at: None,
        completed_artifacts: vec![ContentArtifactKind::SignalPush],
        in_flight_artifacts: vec![ContentArtifactKind::SocialFeed],
        failed_artifacts: Vec::new(),
    };

    assert_eq!(
        evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
        ContentSupplyDecision::Request {
            artifact: ContentArtifactKind::SocialStory,
            attempt: 0,
            confidence: Confidence::saturating_from_basis_points(9_500),
        }
    );
}

#[test]
fn a_year_old_event_is_stale_but_a_year_old_video_is_share_material() {
    let old_event = ContentSupplySnapshot {
        promotion_excluded_platforms: Vec::new(),
        source_id: ContentSourceId::new(),
        source_kind: ContentSourceKind::Event,
        source_version: 1,
        occurred_at: now() - Duration::days(365),
        expires_at: now() + Duration::days(10),
        communication_enabled: None,
        press_enabled: None,
        release_tier: None,
        completed_artifacts: Vec::new(),
        in_flight_artifacts: Vec::new(),
        failed_artifacts: Vec::new(),
        social_post: None,
        source_key: String::new(),
        title: String::new(),
        source_url: None,
        source_body: None,
        source_thumbnail_url: None,
        site_origin: None,
        drop_surge_failures: Vec::new(),
        surge_requested_at: None,
    };
    let old_video = ContentSupplySnapshot {
        source_kind: ContentSourceKind::Video,
        ..old_event.clone()
    };
    let old_story = ContentSupplySnapshot {
        source_kind: ContentSourceKind::Story,
        ..old_event.clone()
    };

    assert_eq!(
        evaluate_content_supply(&old_event, ContentSupplyPolicy::default(), now()),
        ContentSupplyDecision::Hold(ContentSupplyHoldReason::StaleSource),
    );
    // Video and story are evergreen: only expires_at bounds them.
    assert!(matches!(
        evaluate_content_supply(&old_video, ContentSupplyPolicy::default(), now()),
        ContentSupplyDecision::Request { .. }
    ));
    assert!(matches!(
        evaluate_content_supply(&old_story, ContentSupplyPolicy::default(), now()),
        ContentSupplyDecision::Request { .. }
    ));
}

#[test]
fn a_finished_show_waits_out_its_material_window_before_harvesting() {
    let show = ContentSupplySnapshot {
        promotion_excluded_platforms: Vec::new(),
        source_id: ContentSourceId::new(),
        source_kind: ContentSourceKind::ShowCompleted,
        source_version: 1,
        occurred_at: now() - Duration::hours(20),
        expires_at: now() + Duration::days(30),
        communication_enabled: None,
        press_enabled: None,
        release_tier: None,
        completed_artifacts: Vec::new(),
        in_flight_artifacts: Vec::new(),
        failed_artifacts: Vec::new(),
        social_post: None,
        source_key: String::new(),
        title: String::new(),
        source_url: None,
        source_body: None,
        source_thumbnail_url: None,
        site_origin: None,
        drop_surge_failures: Vec::new(),
        surge_requested_at: None,
    };

    // Twenty hours in, the night is over but the capture plan's material
    // is still being collected: nothing drafts from it yet.
    assert_eq!(
        evaluate_content_supply(&show, ContentSupplyPolicy::default(), now()),
        ContentSupplyDecision::Hold(ContentSupplyHoldReason::HarvestPending),
    );

    // Once the window closes the recap artifact is demanded first — the
    // night's own record before the social reuse of it.
    let mut collected = show.clone();
    collected.occurred_at = now() - Duration::hours(80);
    assert!(matches!(
        evaluate_content_supply(&collected, ContentSupplyPolicy::default(), now()),
        ContentSupplyDecision::Request {
            artifact: ContentArtifactKind::PostShowRecap,
            ..
        }
    ));

    // The gate is kind-scoped: events and releases still draft the moment
    // they land, with no collection window to wait out.
    let mut event = show;
    event.source_kind = ContentSourceKind::Event;
    assert!(matches!(
        evaluate_content_supply(&event, ContentSupplyPolicy::default(), now()),
        ContentSupplyDecision::Request { .. }
    ));
}

fn release_snapshot() -> ContentSupplySnapshot {
    ContentSupplySnapshot {
        promotion_excluded_platforms: Vec::new(),
        source_id: ContentSourceId::new(),
        source_kind: ContentSourceKind::Release,
        source_version: 1,
        occurred_at: now() - Duration::days(1),
        expires_at: now() + Duration::days(10),
        communication_enabled: Some(true),
        press_enabled: Some(true),
        release_tier: Some(ReleaseTier::Single),
        completed_artifacts: Vec::new(),
        in_flight_artifacts: Vec::new(),
        failed_artifacts: Vec::new(),
        social_post: None,
        source_key: String::new(),
        title: String::new(),
        source_url: None,
        source_body: None,
        source_thumbnail_url: None,
        site_origin: None,
        drop_surge_failures: Vec::new(),
        surge_requested_at: None,
    }
}

#[test]
fn a_release_with_communication_off_owes_no_fan_facing_artifact() {
    let mut snapshot = release_snapshot();
    snapshot.communication_enabled = Some(false);

    // The operator's own switch holds the whole fan-facing chain; the
    // press hook still stands because press is a different switch and it
    // reaches journalists, not fans.
    assert!(matches!(
        evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
        ContentSupplyDecision::Request {
            artifact: ContentArtifactKind::PressHook,
            ..
        }
    ));

    snapshot.completed_artifacts = vec![ContentArtifactKind::PressHook];
    assert_eq!(
        evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
        ContentSupplyDecision::Hold(ContentSupplyHoldReason::Complete),
    );
}

#[test]
fn a_release_with_press_off_never_owes_a_press_hook() {
    let mut snapshot = release_snapshot();
    snapshot.press_enabled = Some(false);

    // Signal still comes first — the switch only mutes the press side.
    assert!(matches!(
        evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
        ContentSupplyDecision::Request {
            artifact: ContentArtifactKind::SignalPush,
            ..
        }
    ));

    snapshot.completed_artifacts = vec![
        ContentArtifactKind::SignalPush,
        ContentArtifactKind::SocialFeed,
        ContentArtifactKind::SocialStory,
        ContentArtifactKind::NewsletterBlock,
    ];
    assert_eq!(
        evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
        ContentSupplyDecision::Hold(ContentSupplyHoldReason::Complete),
        "press off means the chain completes without the hook"
    );
}

#[test]
fn a_filler_release_is_posted_but_never_pitched() {
    let mut snapshot = release_snapshot();
    snapshot.release_tier = Some(ReleaseTier::Filler);

    // The owned-channel chain still runs — posting the demo is the point
    // of the tier — but a demo owes no press hook.
    snapshot.completed_artifacts = vec![
        ContentArtifactKind::SignalPush,
        ContentArtifactKind::SocialFeed,
        ContentArtifactKind::SocialStory,
        ContentArtifactKind::NewsletterBlock,
    ];
    assert_eq!(
        evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
        ContentSupplyDecision::Hold(ContentSupplyHoldReason::Complete),
    );
}

#[test]
fn a_synced_social_post_without_facts_cannot_relay() {
    // A source row with no social_post facts has nothing to carry — the
    // relay cannot invent a title or link, so it holds rather than send
    // an empty share.
    let snapshot = ContentSupplySnapshot {
        promotion_excluded_platforms: Vec::new(),
        source_id: ContentSourceId::new(),
        source_kind: ContentSourceKind::SocialPost,
        source_version: 1,
        occurred_at: now() - Duration::hours(6),
        expires_at: now() + Duration::days(44),
        communication_enabled: None,
        press_enabled: None,
        release_tier: None,
        completed_artifacts: Vec::new(),
        in_flight_artifacts: Vec::new(),
        failed_artifacts: Vec::new(),
        social_post: None,
        source_key: String::new(),
        title: String::new(),
        source_url: None,
        source_body: None,
        source_thumbnail_url: None,
        site_origin: None,
        drop_surge_failures: Vec::new(),
        surge_requested_at: None,
    };

    assert_eq!(
        evaluate_content_supply(&snapshot, ContentSupplyPolicy::default(), now()),
        ContentSupplyDecision::Hold(ContentSupplyHoldReason::Complete),
    );
}

fn fact(resonance: Option<PostResonance>) -> SocialPostFact {
    SocialPostFact {
        title: "Gramy 17.10 w Gorzowie.".to_owned(),
        url: None,
        platform: "instagram".to_owned(),
        body: None,
        media_url: None,
        media_id: None,
        media_type: None,
        thumbnail_url: None,
        acquired_fans: 0,
        resonance,
    }
}

#[test]
fn only_posts_that_landed_at_home_go_to_communities() {
    let posted = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
    let settled = posted + Duration::hours(40);
    let above = PostResonance {
        engagement: 80,
        peer_median: Some(50),
        peers: 10,
        ..Default::default()
    };
    let below = PostResonance {
        engagement: 30,
        peer_median: Some(50),
        peers: 10,
        ..Default::default()
    };
    assert!(resonates_for_communities(
        &fact(Some(above)),
        posted,
        settled
    ));
    assert!(!resonates_for_communities(
        &fact(Some(below)),
        posted,
        settled
    ));
    // Too early to tell, however well it is doing.
    assert!(!resonates_for_communities(
        &fact(Some(above)),
        posted,
        posted + Duration::hours(6)
    ));
    // Never read and no conversion: fail closed.
    assert!(!resonates_for_communities(&fact(None), posted, settled));
    // A quiet post that created a real fan has stronger evidence than likes.
    let converting = SocialPostFact {
        acquired_fans: 1,
        ..fact(None)
    };
    assert!(resonates_for_communities(&converting, posted, settled));
    // Even conversion evidence waits for the same settle window; the system
    // must not stampede a fresh post before its normal home-audience read.
    assert!(!resonates_for_communities(
        &converting,
        posted,
        posted + Duration::hours(6)
    ));
    // A new account with no history: any real engagement is enough, none is not.
    let first = PostResonance {
        engagement: 4,
        peer_median: None,
        peers: 0,
        ..Default::default()
    };
    assert!(resonates_for_communities(
        &fact(Some(first)),
        posted,
        settled
    ));
    let silent = PostResonance {
        engagement: 0,
        peer_median: None,
        peers: 0,
        ..Default::default()
    };
    assert!(!resonates_for_communities(
        &fact(Some(silent)),
        posted,
        settled
    ));
}

#[test]
fn reach_and_watch_time_decide_when_the_platform_reports_them() {
    let posted = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
    let settled = posted + Duration::hours(40);
    let with = |rate: i64, watch: Option<i64>| PostResonance {
        engagement: 10,
        peer_median: Some(500), // raw engagement alone would refuse it
        peers: 10,
        rate_per_mille: Some(rate),
        peer_rate_median: Some(40),
        rate_peers: 8,
        watch_ms: watch,
        peer_watch_median: Some(4_000),
    };
    // Shown to few people, but those people engaged at a high rate.
    assert!(resonates_for_communities(
        &fact(Some(with(55, None))),
        posted,
        settled
    ));
    // Low rate, and watched no longer than usual: not spread.
    assert!(!resonates_for_communities(
        &fact(Some(with(20, Some(4_100)))),
        posted,
        settled
    ));
    // Low rate, but held attention 25%+ longer than usual: the hook worked.
    assert!(resonates_for_communities(
        &fact(Some(with(20, Some(5_000)))),
        posted,
        settled
    ));
}

#[test]
fn an_outlier_stays_relayable_for_a_week_and_an_ordinary_post_does_not() {
    let now = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
    let snapshot = |resonance: PostResonance, acquired_fans: u32| ContentSupplySnapshot {
        promotion_excluded_platforms: Vec::new(),
        source_id: crate::ContentSourceId::new(),
        source_kind: ContentSourceKind::SocialPost,
        source_version: 1,
        occurred_at: now - Duration::days(5),
        expires_at: now + Duration::days(30),
        communication_enabled: None,
        press_enabled: None,
        release_tier: None,
        completed_artifacts: Vec::new(),
        in_flight_artifacts: Vec::new(),
        failed_artifacts: Vec::new(),
        social_post: Some(SocialPostFact {
            acquired_fans,
            ..fact(Some(resonance))
        }),
        source_key: String::new(),
        title: String::new(),
        source_url: None,
        source_body: None,
        source_thumbnail_url: None,
        site_origin: None,
        drop_surge_failures: Vec::new(),
        surge_requested_at: None,
    };
    let outlier = PostResonance {
        engagement: 120,
        peer_median: Some(50),
        peers: 10,
        ..Default::default()
    };
    let ordinary = PostResonance {
        engagement: 60,
        ..outlier
    };
    assert!(is_outlier(&outlier));
    assert!(!is_outlier(&ordinary));
    let policy = ContentSupplyPolicy::default();
    assert!(matches!(
        evaluate_content_supply(&snapshot(outlier, 0), policy, now),
        ContentSupplyDecision::Relay { .. }
    ));
    assert_eq!(
        evaluate_content_supply(&snapshot(ordinary, 0), policy, now),
        ContentSupplyDecision::Hold(ContentSupplyHoldReason::Complete)
    );
    assert!(matches!(
        evaluate_content_supply(&snapshot(ordinary, 1), policy, now),
        ContentSupplyDecision::Relay { .. }
    ));
}
