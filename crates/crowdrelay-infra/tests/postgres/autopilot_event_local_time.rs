//! A show is announced on the clock of the room it plays in.
//!
//! `events.starts_at` is stored in UTC. Until 2026-09-27 every fan email and
//! letter formatted it as read, so a 20:00 Warsaw show went out to fans as
//! "start o 18:00", and a show after midnight carried the previous day's
//! date. This pins the fan campaign read — the copy a fan receives verbatim.

use std::time::Duration;

use crate::common;
use crowdrelay_application::autopilot::AutopilotDecisionRepository;
use crowdrelay_domain::{WorkspaceId, campaign_lifecycle::EventCampaignPhase};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use time::macros::datetime;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_day_of_email_quotes_the_local_start() -> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("local-time-{suffix}"))
        .bind("Local Time Tests")
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

    // 18:00 UTC on a CEST evening is 20:00 in the room; 23:30 UTC in October
    // is 01:30 the next day.
    let evening = Uuid::now_v7();
    let late = Uuid::now_v7();
    for (id, slug, starts_at) in [
        (evening, "evening", datetime!(2026-09-30 18:00 UTC)),
        (late, "late", datetime!(2026-10-09 23:30 UTC)),
    ] {
        sqlx::query(
            "INSERT INTO events (id, workspace_id, slug, title, timezone, starts_at, status, published_at)
             VALUES ($1, $2, $3, 'Koncert', 'Europe/Warsaw', $4, 'published', $5)",
        )
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(slug)
        .bind(starts_at)
        .bind(datetime!(2026-09-01 00:00 UTC))
        .execute(&pool)
        .await?;
    }

    let now = datetime!(2026-09-30 08:00 UTC);
    let snapshots = repository
        .load_event_campaign_snapshots(workspace_id, now)
        .await?;
    let evening_snapshot = snapshots
        .iter()
        .find(|snapshot| snapshot.event_id.into_uuid() == evening)
        .expect("the evening show is in the window");
    assert_eq!(evening_snapshot.starts_at, datetime!(2026-09-30 18:00 UTC));
    let copy = EventCampaignPhase::DayOf.compose(evening_snapshot);
    assert!(
        copy.body.contains("start o 20:00"),
        "the day-of email must quote the room's clock: {}",
        copy.body
    );

    let late_snapshot = snapshots
        .iter()
        .find(|snapshot| snapshot.event_id.into_uuid() == late)
        .expect("the late show is in the window");
    let copy = EventCampaignPhase::Announcement.compose(late_snapshot);
    assert!(
        copy.subject.contains("10 października 2026"),
        "a show after midnight carries the local date: {}",
        copy.subject
    );
    Ok(())
}
