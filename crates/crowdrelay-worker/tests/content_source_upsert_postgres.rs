//! The source-sync upserts against a real schema.
//!
//! Both sweeps shipped with a placeholder drift — the SQL numbered the
//! metadata bind `$7` and the lifetime-days bind `$8`, but only seven values
//! were bound, so the integer days landed in the jsonb slot and every
//! release/post upsert failed on every sweep. Nothing unit-visible: the
//! query compiled, and only a live database says a `$8` has no eighth bind.
//! These two tests drive the shipped methods so the drift cannot come back.

use anyhow::{Context, Result};
use crowdrelay_worker::release_source_sync::{ReleaseEntry, ReleaseSourceSyncWorker};
use crowdrelay_worker::social_post_source_sync::{PostEntry, SocialPostSourceSyncWorker};
use sqlx::{Connection, PgConnection, PgPool, postgres::PgPoolOptions};
use time::OffsetDateTime;
use uuid::Uuid;

struct DisposableDatabase {
    admin_url: String,
    name: String,
    pool: PgPool,
}

impl DisposableDatabase {
    async fn create() -> Result<Self> {
        let base_url = std::env::var("CROWDRELAY_TEST_DATABASE_URL")
            .context("CROWDRELAY_TEST_DATABASE_URL must target a disposable database")?;
        let (admin_url, _) = split_database_url(&base_url)?;
        let name = format!("crowdrelay_source_upsert_{}", Uuid::now_v7().simple());
        let mut admin = PgConnection::connect(&admin_url)
            .await
            .context("connect to the maintenance database")?;
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await
            .context("create the disposable database")?;
        drop(admin);

        let (prefix, _) = base_url
            .rsplit_once('/')
            .context("CROWDRELAY_TEST_DATABASE_URL has no database segment")?;
        let database_url = format!("{prefix}/{name}");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await
            .context("connect to the disposable database")?;
        crowdrelay_infra::database::MIGRATOR
            .run(&pool)
            .await
            .context("run migrations on the disposable database")?;
        Ok(Self {
            admin_url,
            name,
            pool,
        })
    }

    async fn drop(self) -> Result<()> {
        drop(self.pool);
        let mut admin = PgConnection::connect(&self.admin_url)
            .await
            .context("connect to the maintenance database for drop")?;
        sqlx::query(&format!(
            "DROP DATABASE {name} WITH (FORCE)",
            name = self.name
        ))
        .execute(&mut admin)
        .await
        .context("drop the disposable database")?;
        Ok(())
    }
}

fn split_database_url(url: &str) -> Result<(String, String)> {
    let (prefix, name) = url
        .rsplit_once('/')
        .context("database URL has no path segment")?;
    Ok((format!("{prefix}/postgres"), name.to_owned()))
}

async fn workspace(pool: &PgPool) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("source-upsert-{}", id.simple()))
        .bind("Source Upsert")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(id)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn release_upsert_writes_and_repeats_idempotently() -> Result<()> {
    let db = DisposableDatabase::create().await?;
    let workspace_id = workspace(&db.pool).await?;
    let worker = ReleaseSourceSyncWorker::new(db.pool.clone(), workspace_id)
        .map_err(|error| anyhow::anyhow!("worker build: {error}"))?;

    let entry = ReleaseEntry {
        external_id: "rel-1".to_owned(),
        title: "Some Album".to_owned(),
        url: "https://bandcamp.example/album/some-album".to_owned(),
        released_at: Some(OffsetDateTime::now_utc()),
        description: Some("liner notes".to_owned()),
        release_type: Some("album".to_owned()),
    };
    worker
        .upsert_release("bandcamp", &entry)
        .await
        .map_err(|error| anyhow::anyhow!("first upsert: {error}"))?;
    // The second write of an unchanged fact is a no-op — same row, version 1.
    worker
        .upsert_release("bandcamp", &entry)
        .await
        .map_err(|error| anyhow::anyhow!("repeat upsert: {error}"))?;

    let (count, max_version): (i64, i64) = sqlx::query_as(
        "SELECT count(*), max(version) FROM viryaos_content_sources
         WHERE workspace_id = $1 AND source_kind = 'release'",
    )
    .bind(workspace_id)
    .fetch_one(&db.pool)
    .await
    .context("count release sources")?;
    assert_eq!(count, 1, "one source row per source_key");
    assert_eq!(
        max_version, 1,
        "an unchanged fact does not bump the version"
    );

    let stored: serde_json::Value =
        sqlx::query_scalar("SELECT metadata FROM viryaos_content_sources WHERE workspace_id = $1")
            .bind(workspace_id)
            .fetch_one(&db.pool)
            .await
            .context("read stored metadata")?;
    assert_eq!(stored["release_type"], "album");

    db.drop().await
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn post_upsert_writes_and_repeats_idempotently() -> Result<()> {
    let db = DisposableDatabase::create().await?;
    let workspace_id = workspace(&db.pool).await?;
    let worker = SocialPostSourceSyncWorker::new(db.pool.clone(), workspace_id, None)
        .map_err(|error| anyhow::anyhow!("worker build: {error}"))?;

    let entry = PostEntry {
        external_id: "post-1".to_owned(),
        title: "New single out now".to_owned(),
        url: Some("https://instagram.example/p/abc".to_owned()),
        posted_at: Some(OffsetDateTime::now_utc()),
        caption: Some("the band's own words".to_owned()),
        media_url: Some("https://cdn.instagram.example/img.jpg".to_owned()),
        media_id: Some("media-1".to_owned()),
        media_type: Some("IMAGE".to_owned()),
        thumbnail_url: None,
    };
    worker
        .upsert_post("instagram", &entry)
        .await
        .map_err(|error| anyhow::anyhow!("first upsert: {error}"))?;
    worker
        .upsert_post("instagram", &entry)
        .await
        .map_err(|error| anyhow::anyhow!("repeat upsert: {error}"))?;

    let (count, max_version): (i64, i64) = sqlx::query_as(
        "SELECT count(*), max(version) FROM viryaos_content_sources
         WHERE workspace_id = $1 AND source_kind = 'social_post'",
    )
    .bind(workspace_id)
    .fetch_one(&db.pool)
    .await
    .context("count social post sources")?;
    assert_eq!(count, 1, "one source row per source_key");
    assert_eq!(
        max_version, 1,
        "an unchanged fact does not bump the version"
    );

    db.drop().await
}
