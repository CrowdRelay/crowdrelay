//! The path from connected fanbases to the brain's belief about the world.
//!
//! This is the loop the whole product rests on — platforms are synced into
//! `viryaos_growth_metric_series`, and the brain reads that to decide what to
//! do — and until now nothing exercised it end to end. Both the growth metric
//! sync worker and the snapshot loader had zero integration coverage, which is
//! how a query naming three nonexistent tables reached the working tree and
//! how "this month" shipped reading an absolute level instead of a delta.
//!
//! What cannot be unit-tested and is asserted here:
//!
//! - The north star reads the platform's own series, not some other platform's.
//! - "This month" is a delta between levels, not the latest level.
//! - A series first observed this month does not report its whole existing
//!   audience as won this month.
//! - Off-platform audience sums every connected platform, using each one's
//!   audience-size key — so Last.fm `playcount` (plays) never lands in a total
//!   that means people.
//! - A platform whose newest reading is old counts as connected but not fresh.

use std::time::Duration;

use crowdrelay_application::autopilot::AutopilotDecisionRepository;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;
use uuid::Uuid;

/// Inserts a series and its observations. `points` are `(captured_at, value)`.
async fn seed_series(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    platform: &str,
    metric_key: &str,
    points: &[(OffsetDateTime, i64)],
) -> Result<(), Box<dyn std::error::Error>> {
    let series_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_growth_metric_series
           (id, workspace_id, platform, metric_key, display_name)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(series_id)
    .bind(workspace_id.into_uuid())
    .bind(platform)
    .bind(metric_key)
    .bind(format!("{platform} {metric_key}"))
    .execute(pool)
    .await?;
    for (captured_at, value) in points {
        sqlx::query(
            "INSERT INTO viryaos_growth_metric_points
               (workspace_id, series_id, captured_at, value, source)
             VALUES ($1, $2, $3, $4, 'test')",
        )
        .bind(workspace_id.into_uuid())
        .bind(series_id)
        .bind(captured_at)
        .bind(value)
        .execute(pool)
        .await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn connected_platforms_reach_the_brain_as_audience_and_north_star()
-> Result<(), Box<dyn std::error::Error>> {
    let database_url =
        std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").map_err(|error| {
            format!(
                "CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL must target a disposable database: {error}"
            )
        })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("world-model-e2e-{suffix}"))
        .bind("World model E2E")
        .execute(&pool)
        .await?;

    // The tenant optimizes YouTube subscribers.
    sqlx::query(
        "INSERT INTO tenant_settings (workspace_id, key, value)
         VALUES ($1, 'north_star_metric', 'youtube_subscribers')",
    )
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;

    let now = OffsetDateTime::now_utc();
    // `date_trunc('month', now())` is the boundary the loader uses, so anchor
    // the fixture to it rather than to a fixed number of days back.
    let month_start =
        sqlx::query_scalar::<_, OffsetDateTime>("SELECT date_trunc('month', now())::timestamptz")
            .fetch_one(&pool)
            .await?;
    let before_month = month_start - time::Duration::days(2);
    let in_month = month_start + time::Duration::hours(1);

    // YouTube: 1000 at the month boundary, 1100 now. North star +100.
    seed_series(
        &pool,
        workspace_id,
        "youtube",
        "subscribers",
        &[
            (before_month, 1_000),
            (now - time::Duration::hours(1), 1_100),
        ],
    )
    .await?;

    // Bandcamp: first observed *this* month at 250. Its whole audience must
    // not be reported as won this month — its own first reading is the
    // baseline, so it contributes 0 growth, not 250.
    seed_series(
        &pool,
        workspace_id,
        "bandcamp",
        "supporters",
        &[(in_month, 250)],
    )
    .await?;

    // Discord: stale. Counts as connected, but not as fresh.
    seed_series(
        &pool,
        workspace_id,
        "discord",
        "members",
        &[(now - time::Duration::days(30), 400)],
    )
    .await?;

    // Last.fm plays: a large number under a key that measures plays, not
    // people. It must not be summed into the audience.
    seed_series(
        &pool,
        workspace_id,
        "lastfm",
        "playcount",
        &[(now - time::Duration::hours(1), 9_999_999)],
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
        .load_growth_intelligence_snapshots(workspace_id, now)
        .await?;
    let world = &snapshots
        .first()
        .ok_or("the loader returned no snapshots")?
        .world_model;

    assert_eq!(
        world.north_star_current, 1_100,
        "north star must read the YouTube series"
    );
    assert_eq!(
        world.north_star_this_month, 100,
        "north star this-month must be a delta between levels, not the level"
    );

    // 1100 YouTube + 250 Bandcamp + 400 Discord. Last.fm playcount excluded.
    assert_eq!(
        world.off_platform_audience, 1_750,
        "audience must sum audience-size keys only; playcount is plays, not people"
    );
    // YouTube grew 100. Bandcamp started this month so contributes 0. Discord
    // has a single old reading, so it too contributes 0.
    assert_eq!(
        world.off_platform_audience_this_month, 100,
        "a platform first seen this month must not report its whole audience as growth"
    );
    assert_eq!(
        world.connected_platforms, 3,
        "three platforms carry an audience-size series"
    );
    assert_eq!(
        world.fresh_platforms, 2,
        "the platform whose newest reading is 30 days old is connected but not fresh"
    );

    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(workspace_id.into_uuid())
        .execute(&pool)
        .await?;
    Ok(())
}
