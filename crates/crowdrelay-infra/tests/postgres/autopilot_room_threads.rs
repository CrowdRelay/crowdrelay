//! What the band has read in a room, as both community loaders carry it.
//!
//! The community sweep records the threads a room is discussing
//! (`fan_observations`, kind `post`, with the thread's own date and permalink).
//! The engager and the drop surge post only into a room with enough recent
//! threads, so what the loaders hand them decides whether a post is drafted at
//! all. The window literal in the SQL is held to the domain constant here: a
//! thread 14 days old counts, one 15 days old does not, in the database and in
//! the evaluator both.

use crate::common;

use crowdrelay_application::autopilot::AutopilotDecisionRepository;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::room_reading::{PROMPT_THREADS, READ_MAX_AGE_DAYS};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

async fn community(
    pool: &sqlx::PgPool,
    ws: WorkspaceId,
    subreddit: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let place: Uuid = sqlx::query_scalar(
        "INSERT INTO discovery_places
             (id, workspace_id, place_kind, platform, name, url, status, membership_state)
         VALUES ($1,$2,'subreddit','reddit',$3,$4,'active','joined') RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(ws.into_uuid())
    .bind(subreddit)
    .bind(format!("https://www.reddit.com/r/{subreddit}"))
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO agent_outreach_targets
             (id, workspace_id, target_kind, display_name, status, subreddit, place_id,
              screening_verdict)
         VALUES ($1,$2,'community',$3,'promoted',$4,$5,'admitted')",
    )
    .bind(Uuid::now_v7())
    .bind(ws.into_uuid())
    .bind(subreddit)
    .bind(subreddit)
    .bind(place)
    .execute(pool)
    .await?;
    Ok(place)
}

/// One delivered community post is the delivery receipt that proves the
/// platform lane is executable. Without it the loaders treat the lane as
/// unmeasured and probe a single room at a time, holding every other room
/// back — lane routing this file's window assertions are not about.
async fn prove_delivery_lane(
    pool: &sqlx::PgPool,
    ws: WorkspaceId,
) -> Result<(), Box<dyn std::error::Error>> {
    let decision_id: Uuid = sqlx::query_scalar(
        "INSERT INTO autopilot_decisions
             (id, workspace_id, decision_key, context, subject_kind, subject_id,
              decision_kind, confidence_basis_points, disposition, reason,
              input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
         VALUES ($1,$2,$3,'content_supply','target_community',$4,
                 'seed.post',9000,'auto_execute','seeded delivery receipt',
                 '{}','{}','{}',now(),$5) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(ws.into_uuid())
    .bind(format!("seed-lane-decision-{}", Uuid::now_v7()))
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .fetch_one(pool)
    .await?;
    let action_id: Uuid = sqlx::query_scalar(
        "INSERT INTO autopilot_actions
             (id, workspace_id, decision_id, context, action_kind, subject_kind,
              subject_id, idempotency_key, payload, status, finished_at)
         VALUES ($1,$2,$3,'content_supply','community.engage.request','target_community',
                 $4,$5,'{}','succeeded', now()) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(ws.into_uuid())
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .bind(format!("seed-lane-{}", Uuid::now_v7()))
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO community_posts
             (id, workspace_id, action_id, target_id, subreddit, title, body, status, created_at, posted_at)
         VALUES ($1,$2,$3,NULL,'laneproof','lane proof','body','posted',now(),now())",
    )
    .bind(Uuid::now_v7())
    .bind(ws.into_uuid())
    .bind(action_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn observe(
    pool: &sqlx::PgPool,
    ws: WorkspaceId,
    place: Uuid,
    kind: &str,
    fact: &str,
    url: Option<&str>,
    days_ago: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO fan_observations
             (workspace_id, place_id, observed_at, platform, kind, fact, url)
         VALUES ($1,$2, current_date - $3::int, 'reddit', $4, $5, $6)",
    )
    .bind(ws.into_uuid())
    .bind(place)
    .bind(days_ago)
    .bind(kind)
    .bind(fact)
    .bind(url)
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn both_loaders_carry_the_threads_a_room_is_discussing_now()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let ws = WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(ws.into_uuid())
        .bind(format!("rooms-{}", ws.into_uuid().simple()))
        .bind("Rooms")
        .execute(&pool)
        .await?;
    let busy = community(&pool, ws, "busyroom").await?;
    let other = community(&pool, ws, "otherroom").await?;
    community(&pool, ws, "emptyroom").await?;
    prove_delivery_lane(&pool, ws).await?;

    let window = i32::try_from(READ_MAX_AGE_DAYS)?;
    // Eight recent threads (more than the prompt shows), plus everything that
    // must not count.
    for n in 0..8 {
        observe(
            &pool,
            ws,
            busy,
            "post",
            &format!("thread {n}"),
            Some(&format!("https://www.reddit.com/comments/b{n}")),
            n,
        )
        .await?;
    }
    // The same thread seen on two sweeps is one thread, at its latest date.
    observe(
        &pool,
        ws,
        busy,
        "post",
        "thread 0 (edited title)",
        Some("https://www.reddit.com/comments/b0"),
        3,
    )
    .await?;
    observe(
        &pool,
        ws,
        busy,
        "post",
        "too old",
        Some("https://www.reddit.com/comments/old"),
        window + 1,
    )
    .await?;
    // Not a thread: another kind, a row with no permalink, a date in the future.
    observe(
        &pool,
        ws,
        busy,
        "mention",
        "not a post",
        Some("https://www.reddit.com/comments/m"),
        1,
    )
    .await?;
    observe(&pool, ws, busy, "post", "no permalink", None, 1).await?;
    observe(
        &pool,
        ws,
        busy,
        "post",
        "from tomorrow",
        Some("https://www.reddit.com/comments/future"),
        -1,
    )
    .await?;
    // Another room's thread never leaks into this one.
    observe(
        &pool,
        ws,
        other,
        "post",
        "somewhere else",
        Some("https://www.reddit.com/comments/o1"),
        1,
    )
    .await?;

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(1),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);

    let snapshots = repository
        .load_growth_intelligence_snapshots(ws, OffsetDateTime::now_utc())
        .await?;
    let engager = snapshots
        .iter()
        .find(|snapshot| snapshot.template_id == "community-engager")
        .ok_or("no community-engager snapshot")?;
    let relay = repository.load_relay_community_targets(ws).await?;

    let engager_room = |name: &str| {
        engager
            .unengaged_targets
            .iter()
            .find(|target| target.subreddit == name)
            .map(|target| target.recent_threads.clone())
    };
    let relay_room = |name: &str| {
        relay
            .iter()
            .find(|target| target.subreddit == name)
            .map(|target| target.recent_threads.clone())
    };
    for (loader, busy_room, other_room, unread_room) in [
        (
            "engager",
            engager_room("busyroom"),
            engager_room("otherroom"),
            engager_room("emptyroom"),
        ),
        (
            "drop surge",
            relay_room("busyroom"),
            relay_room("otherroom"),
            relay_room("emptyroom"),
        ),
    ] {
        let busy_room = busy_room.ok_or("busy room missing")?;
        let urls: Vec<&str> = busy_room.iter().map(|thread| thread.url.as_str()).collect();
        // Newest first, one per permalink, none outside the window, and only
        // as many as the drafter is shown.
        assert_eq!(busy_room.len(), PROMPT_THREADS, "{loader}: {urls:?}");
        assert!(
            busy_room
                .windows(2)
                .all(|pair| pair[0].posted_on >= pair[1].posted_on),
            "{loader}: not newest first: {urls:?}"
        );
        assert_eq!(
            urls.iter().filter(|url| url.ends_with("/b0")).count(),
            1,
            "{loader}: a thread seen twice is one thread: {urls:?}"
        );
        for banned in ["old", "future", "m"] {
            assert!(
                !urls.iter().any(|url| url.ends_with(&format!("/{banned}"))),
                "{loader}: {banned} must not count: {urls:?}"
            );
        }
        let other_room = other_room.ok_or("other room missing")?;
        assert_eq!(other_room.len(), 1, "{loader}: {other_room:?}");
        assert!(other_room[0].url.ends_with("/o1"));
        assert!(
            unread_room.ok_or("unread room missing")?.is_empty(),
            "{loader}: a room nobody read has no threads, not an error"
        );
    }

    Ok(())
}

/// The window is inclusive at 14 days and closed at 15 — in the database and in
/// the evaluator's own filter alike. (Its own workspace: the relay loader caps
/// the spread at three communities, so these two rooms must not share a pool
/// with the others.)
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_window_is_fourteen_days_inclusive_in_sql_and_in_the_evaluator()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let ws = WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(ws.into_uuid())
        .bind(format!("rooms-edge-{}", ws.into_uuid().simple()))
        .bind("Rooms edge")
        .execute(&pool)
        .await?;
    let window = i32::try_from(READ_MAX_AGE_DAYS)?;
    let on_the_edge = community(&pool, ws, "edgeroom").await?;
    let just_past = community(&pool, ws, "pastroom").await?;
    prove_delivery_lane(&pool, ws).await?;
    for n in 0..3 {
        observe(
            &pool,
            ws,
            on_the_edge,
            "post",
            &format!("edge {n}"),
            Some(&format!("https://www.reddit.com/comments/e{n}")),
            window,
        )
        .await?;
        observe(
            &pool,
            ws,
            just_past,
            "post",
            &format!("past {n}"),
            Some(&format!("https://www.reddit.com/comments/p{n}")),
            window + 1,
        )
        .await?;
    }
    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(1),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);
    let snapshots = repository
        .load_growth_intelligence_snapshots(ws, OffsetDateTime::now_utc())
        .await?;
    let engager = snapshots
        .iter()
        .find(|snapshot| snapshot.template_id == "community-engager")
        .ok_or("no community-engager snapshot")?;
    let relay = repository.load_relay_community_targets(ws).await?;
    let today = OffsetDateTime::now_utc().date();
    for (loader, edge, past) in [
        (
            "engager",
            engager
                .unengaged_targets
                .iter()
                .find(|t| t.subreddit == "edgeroom")
                .map(|t| t.recent_threads.clone()),
            engager
                .unengaged_targets
                .iter()
                .find(|t| t.subreddit == "pastroom")
                .map(|t| t.recent_threads.clone()),
        ),
        (
            "drop surge",
            relay
                .iter()
                .find(|t| t.subreddit == "edgeroom")
                .map(|t| t.recent_threads.clone()),
            relay
                .iter()
                .find(|t| t.subreddit == "pastroom")
                .map(|t| t.recent_threads.clone()),
        ),
    ] {
        let edge = edge.ok_or("edge room missing")?;
        assert_eq!(edge.len(), 3, "{loader}: day {window} counts");
        assert_eq!(
            crowdrelay_domain::room_reading::counted(&edge, today).len(),
            3,
            "{loader}: the evaluator agrees the edge counts"
        );
        assert!(
            past.ok_or("past room missing")?.is_empty(),
            "{loader}: day {} does not count",
            window + 1
        );
    }
    Ok(())
}
