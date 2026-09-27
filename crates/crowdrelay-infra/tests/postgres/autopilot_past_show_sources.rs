//! A show that already happened is not promoted, and its harvest is dated by
//! the night.
//!
//! On 2026-09-24 past shows were imported. The projection trigger gave each
//! one a fresh `event` source (pre-show listing, newsletter block, push…) and
//! a `show_completed` source stamped with the import time, so the brain asked
//! for listings of nights in February and recaps of nights in July.

use std::time::Duration;

use crate::common;
use crowdrelay_application::autopilot::AutopilotDecisionRepository;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use time::OffsetDateTime;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn past_shows_owe_no_promotion() -> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let ws = workspace_id.into_uuid();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(ws)
        .bind(format!("past-shows-{}", ws.simple()))
        .bind("Past Show Tests")
        .execute(&pool)
        .await?;
    let repository = PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: database_url,
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        },
    );
    let now = OffsetDateTime::now_utc();

    let upcoming = Uuid::now_v7();
    let tonight = Uuid::now_v7();
    let stale_published = Uuid::now_v7();
    let imported_completed = Uuid::now_v7();
    for (id, slug, status, starts_at) in [
        (
            upcoming,
            "upcoming",
            "published",
            now + time::Duration::days(10),
        ),
        // Date-only shows are stored at midnight UTC: the day-of posts still
        // belong to them.
        (
            tonight,
            "tonight",
            "published",
            now - time::Duration::hours(6),
        ),
        (
            stale_published,
            "stale",
            "published",
            now - time::Duration::days(60),
        ),
        (
            imported_completed,
            "imported",
            "completed",
            now - time::Duration::days(70),
        ),
    ] {
        sqlx::query(
            "INSERT INTO events (id, workspace_id, slug, title, timezone, starts_at, status, published_at)
             VALUES ($1, $2, $3, $3, 'Europe/Warsaw', $4, $5, $6)",
        )
        .bind(id)
        .bind(ws)
        .bind(slug)
        .bind(starts_at)
        .bind(status)
        .bind(now - time::Duration::days(90))
        .execute(&pool)
        .await?;
    }

    let event_sources: Vec<Uuid> = repository
        .load_content_supply_snapshots(workspace_id, now)
        .await?
        .into_iter()
        .filter(|snapshot| {
            snapshot.source_kind == crowdrelay_domain::content_supply::ContentSourceKind::Event
        })
        .map(|snapshot| snapshot.source_id.into_uuid())
        .collect();
    // The trigger keys an event's source by the event id.
    assert!(
        event_sources.contains(&upcoming),
        "an upcoming show is promoted"
    );
    assert!(
        event_sources.contains(&tonight),
        "tonight's date-only show keeps its day-of posts"
    );
    assert!(
        !event_sources.contains(&stale_published),
        "a show two months gone owes no listing"
    );
    assert!(
        !event_sources.contains(&imported_completed),
        "an imported past show owes no listing"
    );

    let (occurred_at, expires_at, starts_at) =
        sqlx::query_as::<_, (OffsetDateTime, OffsetDateTime, OffsetDateTime)>(
            "SELECT source.occurred_at, source.expires_at, event.starts_at
             FROM content_sources AS source
             JOIN events AS event
               ON event.workspace_id = source.workspace_id
              AND source.source_key = 'show_completed:' || event.id::text
             WHERE source.workspace_id = $1 AND event.id = $2",
        )
        .bind(ws)
        .bind(imported_completed)
        .fetch_one(&pool)
        .await?;
    assert_eq!(occurred_at, starts_at, "the harvest is dated by the night");
    assert_eq!(expires_at, starts_at + time::Duration::days(45));
    Ok(())
}

/// A deleted show takes its sources with it. The 2026-09-24 duplicates were
/// deleted by hand and their sources kept being promoted, twice over for the
/// shows they duplicated.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_deleted_show_retires_its_sources() -> Result<(), Box<dyn std::error::Error>> {
    let (pool, _) = common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let ws = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $2)")
        .bind(ws)
        .bind(format!("deleted-show-{}", ws.simple()))
        .execute(&pool)
        .await?;
    let now = OffsetDateTime::now_utc();
    let duplicate = Uuid::now_v7();
    let kept = Uuid::now_v7();
    for (id, slug, status) in [
        (duplicate, "duplicate", "completed"),
        (kept, "kept", "completed"),
    ] {
        sqlx::query(
            "INSERT INTO events (id, workspace_id, slug, title, timezone, starts_at, status, published_at)
             VALUES ($1, $2, $3, $3, 'Europe/Warsaw', $4, $5, $4)",
        )
        .bind(id)
        .bind(ws)
        .bind(slug)
        .bind(now - time::Duration::days(3))
        .bind(status)
        .execute(&pool)
        .await?;
    }
    let live = |id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FILTER (WHERE active)::bigint FROM content_sources
                 WHERE workspace_id = $1
                   AND source_key IN ('event:' || $2::text, 'show_completed:' || $2::text)",
            )
            .bind(ws)
            .bind(id)
            .fetch_one(&pool)
            .await
        }
    };
    assert_eq!(
        live(duplicate).await?,
        2,
        "the projection wrote both sources"
    );

    sqlx::query("DELETE FROM events WHERE workspace_id = $1 AND id = $2")
        .bind(ws)
        .bind(duplicate)
        .execute(&pool)
        .await?;
    assert_eq!(live(duplicate).await?, 0, "a deleted show promotes nothing");
    assert_eq!(
        live(kept).await?,
        2,
        "only the deleted show's sources retire"
    );
    Ok(())
}
