//! Sprint 3.5a persistence contract, against a real schema.
//!
//! The unit tests prove the domain rules; this target proves the table
//! behaviour the unit tests cannot see — the dedup indexes, the
//! `WHERE status = expected` transition guards under a real `UPDATE`, and
//! that every row a second workspace asks for comes back empty.

use crowdrelay_domain::{
    WorkspaceId, WorkspaceMemberId,
    content_engine::{
        ArcStatus, PeerStatus, PeerTier, ProductionEventKind, ProductionEventStatus,
        SuggestionOutcomeKind,
    },
};
use crowdrelay_infra::content_engine::{
    ContentEngineError, NewArc, NewCapturePlan, NewFanObservation, NewOutcome, NewPeer,
    NewPeerObservation, NewProductionEvent, NewSuggestion, PostgresContentEngineRepository,
};
use serde_json::json;
use sqlx::{PgPool, postgres::PgPoolOptions};
use time::{Date, Month};

fn date(year: i32, month: u8, day: u8) -> Date {
    Date::from_calendar_date(year, Month::try_from(month).expect("valid month"), day)
        .expect("valid date")
}

async fn repository()
-> Result<(PostgresContentEngineRepository, PgPool), Box<dyn std::error::Error>> {
    let database_url = std::env::var("CROWDRELAY_TEST_DATABASE_URL")
        .map_err(|e| format!("CROWDRELAY_TEST_DATABASE_URL must be configured: {e}"))?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    Ok((PostgresContentEngineRepository::new(pool.clone()), pool))
}

/// Every workspace table now `REFERENCES workspaces(id)` — a test workspace
/// has to exist before rows under it can land.
async fn seed_workspace(
    pool: &PgPool,
    workspace_id: WorkspaceId,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!(
            "content-engine-{}",
            workspace_id.into_uuid().simple()
        ))
        .bind("Content Engine Test")
        .execute(pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn catalogue_is_seeded_and_global() -> Result<(), Box<dyn std::error::Error>> {
    let (repo, _pool) = repository().await?;
    let entries = repo.list_format_entries().await?;
    assert!(
        entries.len() >= 30,
        "the seeded catalogue should carry §4b-2's formats, found {}",
        entries.len()
    );
    let playthrough = entries
        .iter()
        .find(|entry| entry.key == "playthrough")
        .expect("the metal staple is seeded");
    assert_eq!(playthrough.effort_standalone.as_str(), "medium");
    assert_eq!(
        playthrough.effort_marginal.as_str(),
        "low",
        "a playthrough while the shoot is already happening is near-free"
    );
    assert_eq!(playthrough.skill.as_str(), "video");
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn peers_resolve_once_and_dedup_by_name() -> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    let unique = workspace_id.into_uuid().simple().to_string();
    let name = format!("Peer Band {unique}");

    // A scanner proposal lands `proposed` — never observed until confirmed.
    let proposed = repo
        .create_peer(
            workspace_id,
            &NewPeer {
                name: name.clone(),
                handles: json!({"spotify": "peer-band"}),
                tier: PeerTier::NearPeer,
                watch_for: vec!["Format".to_owned(), "format".to_owned()],
                why: "same circuit".to_owned(),
                proposed_by: "release-scanner".to_owned(),
                confirmed: false,
            },
        )
        .await?
        .expect("first insert lands");
    assert_eq!(proposed.status, PeerStatus::Proposed);
    assert_eq!(
        proposed.watch_for,
        vec!["format".to_owned()],
        "watch_for is normalized and deduplicated"
    );

    // The same name again — scanner retries and operator typos both collapse.
    let duplicate = repo
        .create_peer(
            workspace_id,
            &NewPeer {
                name: name.clone(),
                handles: json!({}),
                tier: PeerTier::Aspirational,
                watch_for: vec![],
                why: "duplicate".to_owned(),
                proposed_by: "operator".to_owned(),
                confirmed: true,
            },
        )
        .await?;
    assert!(duplicate.is_none(), "the name index makes a repeat a no-op");

    // Proposed peers are invisible to sweeps.
    let confirmed_before = repo
        .list_peers(workspace_id, Some(PeerStatus::Confirmed))
        .await?;
    assert!(confirmed_before.is_empty());

    let confirmed = repo
        .resolve_peer(workspace_id, proposed.id, PeerStatus::Confirmed, None)
        .await?;
    assert_eq!(confirmed.status, PeerStatus::Confirmed);
    assert!(confirmed.confirmed_at.is_some());

    // A confirmed peer cannot be rejected afterwards — one way only.
    let err = repo
        .resolve_peer(
            workspace_id,
            proposed.id,
            PeerStatus::Rejected,
            Some("late"),
        )
        .await
        .expect_err("a decided peer is final");
    assert!(matches!(err, ContentEngineError::InvalidTransition));

    // A rejection without its reason is refused — the reason is the record
    // that stops the same wrong name being proposed twice.
    let second = repo
        .create_peer(
            workspace_id,
            &NewPeer {
                name: format!("Second {unique}"),
                handles: json!({}),
                tier: PeerTier::Lateral,
                watch_for: vec![],
                why: "to reject".to_owned(),
                proposed_by: "release-scanner".to_owned(),
                confirmed: false,
            },
        )
        .await?
        .expect("second peer lands");
    let silent = repo
        .resolve_peer(workspace_id, second.id, PeerStatus::Rejected, None)
        .await
        .expect_err("a reasonless rejection is refused");
    assert!(matches!(silent, ContentEngineError::MissingReason));
    let rejected = repo
        .resolve_peer(
            workspace_id,
            second.id,
            PeerStatus::Rejected,
            Some("wrong genre"),
        )
        .await?;
    assert_eq!(rejected.rejection_reason.as_deref(), Some("wrong genre"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn observations_dedup_the_same_fact() -> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    let peer = repo
        .create_peer(
            workspace_id,
            &NewPeer {
                name: format!("Observed {}", workspace_id.into_uuid().simple()),
                handles: json!({}),
                tier: PeerTier::Lateral,
                watch_for: vec![],
                why: "watch".to_owned(),
                proposed_by: "operator".to_owned(),
                confirmed: true,
            },
        )
        .await?
        .expect("peer lands");

    let fact = NewPeerObservation {
        peer_id: peer.id,
        observed_at: date(2026, 9, 10),
        platform: "youtube".to_owned(),
        kind: "post".to_owned(),
        fact: "posted a playthrough, 1.2M views in 9 days".to_owned(),
        url: Some("https://example.test/v/1".to_owned()),
        metrics: json!({"views": 1_200_000}),
    };
    let first = repo.record_observation(workspace_id, &fact).await?;
    let second = repo.record_observation(workspace_id, &fact).await?;
    assert!(first.is_some());
    assert_eq!(second, None, "a repeated sweep records the fact once");

    let tail = repo.recent_observations(workspace_id, 10).await?;
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].fact, fact.fact);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn capture_plans_issue_before_the_day() -> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    let event = repo
        .create_production_event(
            workspace_id,
            &NewProductionEvent {
                kind: ProductionEventKind::Shoot,
                title: "video shoot".to_owned(),
                scheduled_for: date(2026, 10, 2),
                event_id: None,
                notes: String::new(),
            },
        )
        .await?;
    assert_eq!(event.status, ProductionEventStatus::Scheduled);

    let upcoming = repo
        .upcoming_production_events(workspace_id, date(2026, 9, 1))
        .await?;
    assert_eq!(upcoming.len(), 1);

    let member = WorkspaceMemberId::new();
    let plan = repo
        .create_capture_plan(
            workspace_id,
            &NewCapturePlan {
                production_event_id: event.id,
                items: json!([{"item": "10 min handheld", "skill": "video"}]),
                assignee_member_id: None,
            },
        )
        .await?;
    assert!(plan.issued_at.is_none(), "a draft was never issued");

    let issued = repo
        .issue_capture_plan(workspace_id, plan.id, member)
        .await?;
    assert!(issued.issued_at.is_some());
    assert_eq!(issued.assignee_member_id, Some(member));

    // Issuing twice must fail — the reminder keys off the first issue.
    let err = repo
        .issue_capture_plan(workspace_id, plan.id, member)
        .await
        .expect_err("an issued plan does not re-issue");
    assert!(matches!(err, ContentEngineError::InvalidTransition));

    // Completing the event honours the status graph.
    repo.set_production_event_status(
        workspace_id,
        event.id,
        ProductionEventStatus::Scheduled,
        ProductionEventStatus::Done,
    )
    .await?;
    let stuck = repo
        .set_production_event_status(
            workspace_id,
            event.id,
            ProductionEventStatus::Scheduled,
            ProductionEventStatus::Cancelled,
        )
        .await;
    assert!(
        matches!(stuck, Err(ContentEngineError::InvalidTransition)),
        "a done event cannot be re-scheduled or cancelled"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn arcs_approve_as_a_whole_and_suggestions_resolve_with_outcomes()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    let arc = repo
        .create_arc(
            workspace_id,
            &NewArc {
                title: "autumn push".to_owned(),
                summary: "three beats around the single".to_owned(),
                horizon_start: Some(date(2026, 9, 15)),
                horizon_end: Some(date(2026, 11, 15)),
                spine: json!([{"at": "2026-10-02", "beat": "playthrough"}]),
                evidence: json!({"peer_observations": [1, 2]}),
            },
        )
        .await?;
    assert_eq!(arc.status, ArcStatus::Proposed);

    // Skipping approval is not a legal move.
    let skip = repo
        .transition_arc(
            workspace_id,
            arc.id,
            ArcStatus::Proposed,
            ArcStatus::Active,
            None,
        )
        .await;
    assert!(matches!(skip, Err(ContentEngineError::InvalidTransition)));

    let approved = repo
        .transition_arc(
            workspace_id,
            arc.id,
            ArcStatus::Proposed,
            ArcStatus::Approved,
            Some("operator"),
        )
        .await?;
    assert!(approved.approved_at.is_some());
    assert_eq!(approved.approved_by.as_deref(), Some("operator"));

    let suggestion = repo
        .create_suggestion(
            workspace_id,
            &NewSuggestion {
                arc_id: Some(arc.id),
                format_key: Some("playthrough".to_owned()),
                concept: "film the playthrough during the shoot".to_owned(),
                reason: "peer X's playthrough outperformed 4:1".to_owned(),
                evidence: json!({"peer_observations": [1]}),
                suggested_after: Some(date(2026, 10, 2)),
                suggested_before: Some(date(2026, 10, 16)),
                effort: None,
                proposed_assignee_member_id: None,
                distribution_promise: json!({"consented_fans": 340, "communities": ["r/metal"]}),
                expires_at: None,
            },
        )
        .await?;
    let open = repo.list_open_suggestions(workspace_id).await?;
    assert_eq!(open.len(), 1);

    let outcome = repo
        .resolve_suggestion(
            workspace_id,
            &NewOutcome {
                suggestion_id: suggestion.id,
                outcome: SuggestionOutcomeKind::Declined,
                decided_by: Some("operator".to_owned()),
                reason: Some("not for us".to_owned()),
                results: json!({}),
            },
        )
        .await?;
    assert_eq!(outcome.outcome, SuggestionOutcomeKind::Declined);
    assert_eq!(outcome.reason.as_deref(), Some("not for us"));

    // The decision and its outcome committed together, and a decided
    // suggestion cannot be resolved a second time.
    assert!(repo.list_open_suggestions(workspace_id).await?.is_empty());
    let again = repo
        .resolve_suggestion(
            workspace_id,
            &NewOutcome {
                suggestion_id: suggestion.id,
                outcome: SuggestionOutcomeKind::Done,
                decided_by: None,
                reason: None,
                results: json!({}),
            },
        )
        .await;
    assert!(matches!(again, Err(ContentEngineError::InvalidTransition)));

    let history = repo
        .outcomes_for_suggestion(workspace_id, suggestion.id)
        .await?;
    assert_eq!(history.len(), 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn a_second_workspace_sees_nothing() -> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let ours = WorkspaceId::new();
    seed_workspace(&pool, ours).await?;
    let theirs = WorkspaceId::new();
    let peer = repo
        .create_peer(
            ours,
            &NewPeer {
                name: format!("Scoped {}", ours.into_uuid().simple()),
                handles: json!({}),
                tier: PeerTier::NearPeer,
                watch_for: vec![],
                why: "ours".to_owned(),
                proposed_by: "operator".to_owned(),
                confirmed: true,
            },
        )
        .await?
        .expect("peer lands");
    repo.record_observation(
        ours,
        &NewPeerObservation {
            peer_id: peer.id,
            observed_at: date(2026, 9, 12),
            platform: "instagram".to_owned(),
            kind: "post".to_owned(),
            fact: "tour announce".to_owned(),
            url: None,
            metrics: json!({}),
        },
    )
    .await?;

    assert!(repo.list_peers(theirs, None).await?.is_empty());
    assert!(repo.recent_observations(theirs, 50).await?.is_empty());
    assert!(
        repo.upcoming_production_events(theirs, date(2020, 1, 1))
            .await?
            .is_empty()
    );
    assert!(repo.list_open_suggestions(theirs).await?.is_empty());
    assert!(repo.list_arcs(theirs, None).await?.is_empty());

    // And the other workspace cannot reach our rows by id either.
    let cross = repo
        .resolve_peer(theirs, peer.id, PeerStatus::Rejected, Some("x"))
        .await;
    assert!(
        matches!(cross, Err(ContentEngineError::InvalidTransition)),
        "the workspace clause makes a foreign id a no-match"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn fan_observations_deduplicate_and_scope_to_the_place()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;

    // Fan rows hang off a discovery_places row — the community the posts
    // were seen in.
    let place_id = sqlx::query_scalar::<_, uuid::Uuid>(
        "INSERT INTO discovery_places (workspace_id, place_kind, platform, name, url)
         VALUES ($1, 'subreddit', 'reddit', 'r/testmetal', 'https://www.reddit.com/r/testmetal')
         RETURNING id",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;

    let post = NewFanObservation {
        place_id,
        observed_at: date(2026, 9, 14),
        platform: "reddit".to_owned(),
        kind: "post".to_owned(),
        fact: "What albums this week?".to_owned(),
        url: Some("https://www.reddit.com/comments/abc123".to_owned()),
        metrics: json!({"score": 412, "comments": 96}),
    };
    assert!(
        repo.record_fan_observation(workspace_id, &post)
            .await?
            .is_some()
    );
    // A post that stays hot reappears in every sweep; the dedup index, not
    // luck, is what keeps the second sighting from becoming a second row.
    assert!(
        repo.record_fan_observation(workspace_id, &post)
            .await?
            .is_none(),
        "the same fact at the same place+date must not record twice"
    );

    let tail = repo.recent_fan_observations(workspace_id, 10).await?;
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].fact, "What albums this week?");
    assert_eq!(tail[0].place_id, place_id);
    assert_eq!(tail[0].metrics["score"], json!(412));

    // Another workspace sees none of it.
    let theirs = WorkspaceId::new();
    seed_workspace(&pool, theirs).await?;
    assert!(repo.recent_fan_observations(theirs, 10).await?.is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn trends_detect_corroboration_and_fade() -> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;

    // Two peers agreeing on a format inside the window is the confirmed
    // case — corroboration is the claim.
    let today = time::OffsetDateTime::now_utc().date();
    let recent = today - time::Duration::days(3);
    for name in ["Peer Alpha", "Peer Beta"] {
        let peer = repo
            .create_peer(
                workspace_id,
                &NewPeer {
                    name: name.to_owned(),
                    handles: json!({}),
                    tier: PeerTier::NearPeer,
                    watch_for: vec![],
                    why: "trend fixture".to_owned(),
                    proposed_by: "operator".to_owned(),
                    confirmed: true,
                },
            )
            .await?
            .expect("peer lands");
        for (day, fact) in [
            (recent, "new playthrough of the single"),
            (today, "another playthrough video"),
        ] {
            repo.record_observation(
                workspace_id,
                &NewPeerObservation {
                    peer_id: peer.id,
                    observed_at: day,
                    platform: "youtube".to_owned(),
                    kind: "video".to_owned(),
                    fact: fact.to_owned(),
                    url: None,
                    metrics: json!({}),
                },
            )
            .await?;
        }
    }

    let live = repo.refresh_trends(workspace_id, today).await?;
    assert!(live > 0, "corroborating facts should produce trends");

    let trends = repo.list_trends(workspace_id).await?;
    let playthrough = trends
        .iter()
        .find(|t| t.pattern == "playthrough")
        .expect("the agreed format is a trend");
    assert_eq!(playthrough.status.as_str(), "confirmed");
    assert_eq!(playthrough.sources, 2);
    assert!(
        playthrough.evidence["peer"].as_array().unwrap().len() >= 3,
        "the evidence links are the claim's receipts"
    );

    // A second workspace shares nothing.
    let theirs = WorkspaceId::new();
    seed_workspace(&pool, theirs).await?;
    assert!(repo.list_trends(theirs).await?.is_empty());

    // When the facts age out of the window, the trend fades instead of
    // vanishing — fading is a fact, not a deletion.
    let later = today + time::Duration::days(60);
    assert_eq!(repo.refresh_trends(workspace_id, later).await?, 0);
    let faded = repo.list_trends(workspace_id).await?;
    assert!(
        faded.iter().all(|t| t.status.as_str() == "faded"),
        "a pattern that stopped appearing says so"
    );
    Ok(())
}
