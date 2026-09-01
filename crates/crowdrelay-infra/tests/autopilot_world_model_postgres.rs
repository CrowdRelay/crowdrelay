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

/// A tenant whose reach is spread across platforms can name the whole
/// portfolio as its north star.
///
/// Before the vocabulary widened, only YouTube, Spotify and Bandsintown could
/// be a north star, so a DJ measured on SoundCloud or a pop act measured on
/// TikTok silently fell back to Signal installs — a metric that means nothing
/// to a tenant who does not use Signal, and one that would have reported them
/// as having no growth at all.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn total_audience_north_star_reads_every_connected_platform()
-> Result<(), Box<dyn std::error::Error>> {
    let database_url = std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL")?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("total-audience-{suffix}"))
        .bind("Total audience")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO tenant_settings (workspace_id, key, value)
         VALUES ($1, 'north_star_metric', 'total_audience')",
    )
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;

    let now = OffsetDateTime::now_utc();
    let month_start =
        sqlx::query_scalar::<_, OffsetDateTime>("SELECT date_trunc('month', now())::timestamptz")
            .fetch_one(&pool)
            .await?;
    let before_month = month_start - time::Duration::days(2);
    let recent = now - time::Duration::hours(1);

    // A DJ's shape: SoundCloud and TikTok, no YouTube, no Signal.
    seed_series(
        &pool,
        workspace_id,
        "soundcloud",
        "followers",
        &[(before_month, 4_000), (recent, 4_600)],
    )
    .await?;
    seed_series(
        &pool,
        workspace_id,
        "tiktok",
        "followers",
        &[(before_month, 10_000), (recent, 10_400)],
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
        world.north_star_current, 15_000,
        "total-audience north star must be the sum across platforms"
    );
    assert_eq!(
        world.north_star_this_month, 1_000,
        "and its growth must be the summed delta, not one platform's"
    );
    assert_eq!(world.off_platform_audience, world.north_star_current);
    Ok(())
}

/// A platform that was never selectable before is now a first-class north star.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_soundcloud_north_star_reads_the_soundcloud_series()
-> Result<(), Box<dyn std::error::Error>> {
    let database_url = std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL")?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("soundcloud-ns-{suffix}"))
        .bind("SoundCloud north star")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO tenant_settings (workspace_id, key, value)
         VALUES ($1, 'north_star_metric', 'soundcloud_followers')",
    )
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;

    let now = OffsetDateTime::now_utc();
    let month_start =
        sqlx::query_scalar::<_, OffsetDateTime>("SELECT date_trunc('month', now())::timestamptz")
            .fetch_one(&pool)
            .await?;
    seed_series(
        &pool,
        workspace_id,
        "soundcloud",
        "followers",
        &[
            (month_start - time::Duration::days(2), 900),
            (now - time::Duration::hours(1), 1_150),
        ],
    )
    .await?;
    // A second platform must not leak into a single-platform north star.
    seed_series(
        &pool,
        workspace_id,
        "instagram",
        "followers",
        &[(now - time::Duration::hours(1), 50_000)],
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

    assert_eq!(world.north_star_current, 1_150, "SoundCloud only");
    assert_eq!(world.north_star_this_month, 250);
    assert_eq!(
        world.off_platform_audience, 51_150,
        "the audience aggregate still spans both platforms"
    );
    Ok(())
}

/// Counting activity must not be mistaken for counting places.
///
/// Both pipeline counters were `COUNT(*)` over a LEFT JOIN onto
/// `community_posts`, so every extra post added another join row and another
/// phantom community or outreach target. The error was invisible while the
/// tables were empty and grew with exactly the activity the brain generates.
///
/// The direction is what makes it worse than an off-by-one: `discovered_communities`
/// is evidence that the brain already has reach, so posting more made the brain
/// believe it needed to find fewer places — the opposite of the truth.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn pipeline_counts_count_places_not_posts() -> Result<(), Box<dyn std::error::Error>> {
    let database_url = std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL")?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("pipeline-counts-{suffix}"))
        .bind("Pipeline counts")
        .execute(&pool)
        .await?;

    // Three communities. Only the first will carry any posts.
    for (name, url) in [
        ("r/Test", "https://reddit.com/r/Test"),
        ("r/Quiet", "https://reddit.com/r/Quiet"),
        ("Some Discord", "https://discord.example/server"),
    ] {
        sqlx::query(
            "INSERT INTO discovery_places
               (workspace_id, place_kind, platform, name, url)
             VALUES ($1, 'subreddit', 'reddit', $2, $3)",
        )
        .bind(workspace_id.into_uuid())
        .bind(name)
        .bind(url)
        .execute(&pool)
        .await?;
    }

    // Two outreach targets, both still proposed.
    let mut target_ids = Vec::new();
    for name in ["Loud Blog", "Quiet Blog"] {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO agent_outreach_targets
               (id, workspace_id, target_kind, display_name, status)
             VALUES ($1, $2, 'press', $3, 'proposed')",
        )
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(name)
        .execute(&pool)
        .await?;
        target_ids.push(id);
    }

    // `community_posts.action_id` is a real foreign key, so the post rows need
    // a decision and an action above them before they can exist at all.
    let decision_id = Uuid::now_v7();
    let subject_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation)
         VALUES ($1, $2, $3, 'outreach', 'discovery_place', $4,
                 'community.post', 5000, 'require_approval', 'fixture',
                 '{}'::jsonb, '{}'::jsonb, '{}'::jsonb)",
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("pipeline-counts-{suffix}"))
    .bind(subject_id)
    .execute(&pool)
    .await?;

    // Four posts, all in one subreddit and all against one target. Under the
    // old queries this read as four communities and four proposed targets.
    // `community_posts.action_id` is unique, so each post needs its own action.
    for index in 0..4 {
        // A partial unique index forbids two in-flight actions on the same
        // subject, so each post targets its own.
        let action_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO viryaos_autopilot_actions
               (id, workspace_id, decision_id, context, action_kind,
                subject_kind, subject_id, idempotency_key, payload, status)
             VALUES ($1, $2, $3, 'outreach', 'community.post',
                     'discovery_place', $4, $5, '{}'::jsonb, 'queued')",
        )
        .bind(action_id)
        .bind(workspace_id.into_uuid())
        .bind(decision_id)
        .bind(Uuid::now_v7())
        .bind(format!("pipeline-counts-{suffix}-{index}"))
        .execute(&pool)
        .await?;

        sqlx::query(
            "INSERT INTO community_posts
               (workspace_id, action_id, target_id, subreddit, title, body,
                status, posted_at)
             VALUES ($1, $2, $3, 'r/Test', $4, 'body', 'posted', now())",
        )
        .bind(workspace_id.into_uuid())
        .bind(action_id)
        .bind(target_ids[0])
        .bind(format!("post {index}"))
        .execute(&pool)
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
        .load_growth_intelligence_snapshots(workspace_id, OffsetDateTime::now_utc())
        .await?;
    let world = &snapshots
        .first()
        .ok_or("the loader returned no snapshots")?
        .world_model;

    assert_eq!(
        world.discovered_communities, 3,
        "three places exist; four posts in one of them must not read as more places"
    );
    assert_eq!(
        world.active_communities, 1,
        "one of the three places has recent posts"
    );
    assert_eq!(
        world.pending_outreach_targets, 2,
        "two targets are proposed; posts against one must not multiply it"
    );

    // No teardown. A trigger mirrors every action into `viryaos_action_ledger`,
    // which is append-only (DELETE raises) and holds an ON DELETE RESTRICT key
    // back to the workspace — so once a workspace has recorded an action, it
    // cannot be deleted. That is the audit guarantee working as intended; the
    // test database is disposable, so the row is simply left behind.
    Ok(())
}

/// The brain must be able to see *where* audience is coming from, not only
/// that it is coming.
///
/// Per-platform growth is what lets the strategy's template order stop being a
/// fixed guess. The series were always there; nothing read them this way, so a
/// tenant whose Telegram was compounding kept being sent to Reddit first.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_world_model_reports_growth_per_platform() -> Result<(), Box<dyn std::error::Error>> {
    let database_url = std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL")?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("platform-growth-{suffix}"))
        .bind("Platform growth")
        .execute(&pool)
        .await?;

    let now = OffsetDateTime::now_utc();
    let month_start =
        sqlx::query_scalar::<_, OffsetDateTime>("SELECT date_trunc('month', now())::timestamptz")
            .fetch_one(&pool)
            .await?;
    let before_month = month_start - time::Duration::days(2);

    // Telegram compounding, YouTube flat, Signal growing.
    seed_series(
        &pool,
        workspace_id,
        "telegram",
        "subscribers",
        &[
            (before_month, 1_000),
            (now - time::Duration::hours(1), 1_400),
        ],
    )
    .await?;
    seed_series(
        &pool,
        workspace_id,
        "youtube",
        "subscribers",
        &[
            (before_month, 5_000),
            (now - time::Duration::hours(1), 5_000),
        ],
    )
    .await?;
    seed_series(
        &pool,
        workspace_id,
        "signal",
        "active_fans",
        &[(before_month, 100), (now - time::Duration::hours(1), 160)],
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

    let find = |platform: &str| {
        world
            .platform_growth
            .iter()
            .find(|entry| entry.platform == platform)
            .cloned()
    };

    let telegram = find("telegram").ok_or("telegram growth missing")?;
    assert_eq!(telegram.audience, 1_400);
    assert_eq!(telegram.gained_this_month, 400);

    let youtube = find("youtube").ok_or("youtube growth missing")?;
    assert_eq!(
        youtube.gained_this_month, 0,
        "a flat platform gained nothing"
    );

    let signal = find("signal").ok_or("signal must be ranked alongside the feeds")?;
    assert_eq!(signal.gained_this_month, 60);

    // The order the brain will actually try, given this evidence. Telegram's
    // 400 on 1400 beats YouTube's flat 5000; Signal's 60 on 160 is weighted up
    // but sits below the evidence floor's full confidence, so the assertion is
    // only that the flat platform loses.
    let strategy = crowdrelay_brain::strategy::GrowthStrategy::AggressiveDiscovery;
    let ranked = strategy.template_priority_for(world);
    let telegram_at = ranked.iter().position(|t| *t == "telegram-scanner");
    let reddit_at = ranked.iter().position(|t| *t == "reddit-scanner");
    assert!(
        telegram_at < reddit_at,
        "the platform returning audience should be tried before one with no measured return: {ranked:?}"
    );
    Ok(())
}
