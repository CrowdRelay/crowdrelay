//! A fresh upload opens one release plan; a re-sync and an old video open none.
//!
//! The watcher used to stop at the `content_sources` row — a new video was
//! shareable but the autopilot never learned a release was happening. The
//! first-insert branch asks the same upsert the control plane uses, so the
//! plan, its audit row and its version land together. These tests pin the
//! three shapes: one plan on insert, no second plan on a re-read, and no
//! plan at all for a video the feed only now surfaces at ten days old.

use crate::common;

include!("video_promotion_refresh.rs");

use anyhow::{Context, Result, anyhow};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use crowdrelay_worker::video_source_sync::{FeedEntry, VideoSourceSyncWorker};
use sqlx::PgPool;
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

async fn fixture() -> Result<(PgPool, VideoSourceSyncWorker, Uuid)> {
    let (pool, url) = common::test_pool_with_url("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let workspace_id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id)
        .bind(format!("video-plan-{}", workspace_id.simple()))
        .bind("Video Plan")
        .execute(&pool)
        .await
        .context("insert workspace")?;
    let repository = PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url,
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        },
    );
    let worker = VideoSourceSyncWorker::new(pool.clone(), workspace_id, None, repository)
        .map_err(|error| anyhow!("worker build: {error}"))?;
    Ok((pool, worker, workspace_id))
}

fn entry(video_id: &str, published: OffsetDateTime) -> FeedEntry {
    FeedEntry {
        video_id: video_id.to_owned(),
        title: format!("Video {video_id}"),
        published: Some(published),
        description: None,
    }
}

async fn plan_count(pool: &PgPool, workspace_id: Uuid) -> Result<i64> {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM release_plans WHERE workspace_id = $1")
        .bind(workspace_id)
        .fetch_one(pool)
        .await
        .context("count release plans")
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_fresh_upload_opens_one_release_plan() -> Result<()> {
    let (pool, worker, workspace_id) = fixture().await?;
    worker
        .upsert_video("UCchan", &entry("freshvid1", OffsetDateTime::now_utc()))
        .await
        .map_err(|error| anyhow!("upsert: {error}"))?;

    assert_eq!(plan_count(&pool, workspace_id).await?, 1);
    let row: (String, String, Option<String>, bool) = sqlx::query_as(
        "SELECT source_key, title, listen_url, active FROM release_plans WHERE workspace_id = $1",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await
    .context("read release plan")?;
    assert_eq!(row.0, "youtube:freshvid1");
    assert_eq!(row.1, "Video freshvid1");
    assert_eq!(row.2.as_deref(), Some("https://youtu.be/freshvid1"));
    assert!(row.3);

    // The audit row names the watcher, not an administrator.
    let actor: String = sqlx::query_scalar(
        "SELECT actor_type FROM operator_actions \
         WHERE workspace_id = $1 AND action = 'upsert_autopilot_release_plan'",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await
    .context("read audit actor")?;
    assert_eq!(actor, "video-watcher");
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_resynced_video_opens_no_second_plan() -> Result<()> {
    let (pool, worker, workspace_id) = fixture().await?;
    worker
        .upsert_video("UCchan", &entry("seenagain", OffsetDateTime::now_utc()))
        .await
        .map_err(|error| anyhow!("first upsert: {error}"))?;
    // The feed re-reads the same upload with a retitled row; the upsert
    // updates in place, `xmax = 0` is false, and no second plan appears.
    let mut again = entry("seenagain", OffsetDateTime::now_utc());
    again.title = "Video seenagain (remastered)".to_owned();
    worker
        .upsert_video("UCchan", &again)
        .await
        .map_err(|error| anyhow!("resync upsert: {error}"))?;

    assert_eq!(plan_count(&pool, workspace_id).await?, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_ten_day_old_video_opens_no_plan() -> Result<()> {
    let (pool, worker, workspace_id) = fixture().await?;
    let published = OffsetDateTime::now_utc() - time::Duration::days(10);
    worker
        .upsert_video("UCchan", &entry("oldupload", published))
        .await
        .map_err(|error| anyhow!("upsert: {error}"))?;

    assert_eq!(plan_count(&pool, workspace_id).await?, 0);
    Ok(())
}
