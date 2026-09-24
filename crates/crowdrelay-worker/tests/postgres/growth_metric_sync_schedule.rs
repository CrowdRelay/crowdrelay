//! The metric sync scheduler's retry contract, against a real schema.
//!
//! Due-ness used to mean one thing only: "no metric point newer than the sync
//! interval". A connection whose provider always fails never records a point,
//! so it was due, failed, and was still due — and `next_due_time` answered
//! `Instant::now()` for exactly that case, which made the loop's `sleep`
//! return instantly. Production's Discogs connection sat in that loop at three
//! to four requests a second against an API already answering 429: 1372
//! failures in seven minutes, and a log so full of them that the single line
//! reporting city geocoding was switched off was buried underneath.
//!
//! The failure state was already recorded on the connection and read by
//! nothing. Both halves of the schedule now read it, and both halves are
//! asserted here — the selection, and the sleep. Neither is observable without
//! a real row, and a test that reimplemented the SQL would prove nothing about
//! the query that actually ships, which is why the two methods are public.

use crate::common;

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use crowdrelay_infra::sensitive_response::SensitiveResponseKey;
use crowdrelay_worker::growth_metric_sync::{FAILURE_RETRY_DELAY, GrowthMetricSyncWorker};
use sqlx::PgPool;
use uuid::Uuid;

/// A worker wired to the pool and to nothing else. Every scheduling decision
/// this test makes is a database read, so the provider credentials are
/// irrelevant — but `new` returns `None` when the process holds none at all,
/// so one is supplied to get an instance back.
fn worker(pool: PgPool) -> Result<GrowthMetricSyncWorker> {
    GrowthMetricSyncWorker::new(
        pool,
        None,
        None,
        "http://agent-service.invalid".to_owned(),
        None,
        None,
        None,
        None,
        Some("discogs-token-for-tests".to_owned()),
        SensitiveResponseKey::derive_from_secret(b"growth-metric-sync-schedule-test"),
        Duration::from_secs(5),
    )
    .context("build the sync worker")?
    .context("worker built as None despite a configured credential")
}

async fn workspace(pool: &PgPool) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("sync-schedule-{}", id.simple()))
        .bind("Sync Schedule")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(id)
}

/// A connected connection whose last attempt failed `failed_ago` in the past,
/// or which has never failed.
async fn connection(
    pool: &PgPool,
    workspace_id: Uuid,
    platform: &str,
    failed_ago: Option<Duration>,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO fanbase_connections (
            id, workspace_id, platform, status, provider_account_id,
            external_account_ref, credential_ref, label, last_sync_failed_at
        )
        VALUES (
            $1, $2, $3, 'connected', $4, $5, $6, $7,
            CASE WHEN $8::bigint IS NULL
                 THEN NULL
                 ELSE now() - ($8::bigint * interval '1 second')
            END
        )
        "#,
    )
    .bind(id)
    .bind(workspace_id)
    .bind(platform)
    .bind(format!("account-{}", id.simple()))
    .bind(format!("ref-{}", id.simple()))
    .bind(format!("cred-{}", id.simple()))
    .bind(format!("{platform} probe"))
    .bind(failed_ago.map(|ago| ago.as_secs() as i64))
    .execute(pool)
    .await
    .context("insert fanbase connection")?;
    Ok(id)
}

/// The bootstrap migrations may seed connections of their own; a scenario only
/// means something when the set is exactly what it inserted.
async fn clear_connections(pool: &PgPool) -> Result<()> {
    sqlx::query("DELETE FROM fanbase_connections")
        .execute(pool)
        .await
        .context("clear seeded connections")?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_connection_that_just_failed_is_not_retried_immediately() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    a_connection_that_just_failed_is_not_retried_immediately_inner(&database).await
}

async fn a_connection_that_just_failed_is_not_retried_immediately_inner(
    pool: &PgPool,
) -> Result<()> {
    clear_connections(pool).await?;
    let workspace_id = workspace(pool).await?;

    // Failed a minute ago: inside the retry delay, so not due.
    connection(pool, workspace_id, "discogs", Some(Duration::from_secs(60))).await?;
    // Failed longer ago than the delay: due again.
    connection(
        pool,
        workspace_id,
        "spotify",
        Some(FAILURE_RETRY_DELAY + Duration::from_secs(60)),
    )
    .await?;
    // Never failed and never synced: due now.
    connection(pool, workspace_id, "youtube", None).await?;

    let worker = worker(pool.clone())?;
    let mut due: Vec<String> = worker
        .find_due_connections()
        .await
        .context("find due connections")?
        .into_iter()
        .map(|conn| conn.platform)
        .collect();
    due.sort();

    ensure!(
        due == vec!["spotify".to_owned(), "youtube".to_owned()],
        "the connection that failed a minute ago must wait out its retry delay; got {due:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn spotify_top_cities_resolve_against_the_city_catalog() -> Result<()> {
    // Its own database: the assertions are about the migration's own seed,
    // and on the shared suite database any sibling's PL city named "Wrocław"
    // (the Latarnik suite needs one) makes the name ambiguous — which the
    // join rightly refuses to guess through.
    let database = common::isolated_database("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("create a migrated database of this test's own");

    let outcome = spotify_city_resolution_inner(&database.pool).await;
    database.drop().await?;
    outcome
}

async fn spotify_city_resolution_inner(pool: &PgPool) -> Result<()> {
    // The seeded catalog carries local names only; Spotify reports English
    // exonyms and undiacriticised spellings. Both directions must join, and
    // a name with no catalog row must resolve to None rather than to a
    // wrong city. The assertions run against the migration's own seed — if
    // the catalog drifts, this test drifts with it, which is the point.
    let (wroclaw_id, wroclaw_name) =
        crowdrelay_worker::growth_metric_sync::resolve_spotify_city(pool, "PL", "Wroclaw")
            .await?
            .context("Wroclaw must resolve through the diacritic fold")?;
    ensure!(
        wroclaw_name == "Wrocław",
        "expected Wrocław, got {wroclaw_name}"
    );

    let (warsaw_id, warsaw_name) =
        crowdrelay_worker::growth_metric_sync::resolve_spotify_city(pool, "PL", "Warsaw")
            .await?
            .context("Warsaw must resolve through the exonym map")?;
    ensure!(
        warsaw_name == "Warszawa",
        "expected Warszawa, got {warsaw_name}"
    );
    ensure!(
        warsaw_id != wroclaw_id,
        "Warsaw and Wroclaw resolved to the same city"
    );

    // Country scoping: a Warsaw outside Poland must not join Warszawa.
    let us =
        crowdrelay_worker::growth_metric_sync::resolve_spotify_city(pool, "US", "Warsaw").await?;
    ensure!(
        us.is_none(),
        "the US Warsaw must not reach across the border to Warszawa"
    );

    let missing =
        crowdrelay_worker::growth_metric_sync::resolve_spotify_city(pool, "PL", "Zakopane").await?;
    ensure!(
        missing.is_none(),
        "a city absent from the catalog must resolve to None, not a guess"
    );

    // ß is a digraph the fold expands to "ss" — Spotify reports the "ss"
    // spelling while the catalog keeps the local one, and the join must
    // still land.
    sqlx::query(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude) \
         VALUES ('weissenfels', 'Weißenfels', 'DE', 51.1989, 11.9683)",
    )
    .execute(pool)
    .await?;
    let (weissenfels_id, weissenfels_name) =
        crowdrelay_worker::growth_metric_sync::resolve_spotify_city(pool, "DE", "Weissenfels")
            .await?
            .context("Weissenfels must resolve through the ß digraph fold")?;
    ensure!(
        weissenfels_name == "Weißenfels",
        "expected Weißenfels, got {weissenfels_name}"
    );
    let also =
        crowdrelay_worker::growth_metric_sync::resolve_spotify_city(pool, "DE", "Weißenfels")
            .await?;
    ensure!(
        also.is_some_and(|(id, _)| id == weissenfels_id),
        "the ß spelling must resolve to the same row"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_worker_sleeps_instead_of_spinning_when_everything_is_backing_off() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    sleeps_instead_of_spinning_inner(&database).await
}

async fn sleeps_instead_of_spinning_inner(pool: &PgPool) -> Result<()> {
    clear_connections(pool).await?;
    let workspace_id = workspace(pool).await?;
    // Both failed five minutes ago and neither has ever recorded a point --
    // the exact shape that produced a zero-length sleep and a hot loop.
    connection(
        pool,
        workspace_id,
        "discogs",
        Some(Duration::from_secs(300)),
    )
    .await?;
    connection(
        pool,
        workspace_id,
        "spotify",
        Some(Duration::from_secs(300)),
    )
    .await?;

    let worker = worker(pool.clone())?;

    let due = worker
        .find_due_connections()
        .await
        .context("find due connections")?;
    ensure!(
        due.is_empty(),
        "nothing should be due while every connection is inside its retry delay; got {} rows",
        due.len()
    );

    let next = worker
        .next_due_time()
        .await
        .context("a next due time must exist while connections are backing off")?;
    let sleep_for = next.saturating_duration_since(tokio::time::Instant::now());
    // Five minutes of the hour have elapsed, so the wake-up is about 55 minutes
    // out. The window is wide because the assertion is "not zero", not "exactly
    // 3300 seconds" -- a zero-length sleep is the entire bug.
    ensure!(
        sleep_for >= FAILURE_RETRY_DELAY - Duration::from_secs(360)
            && sleep_for <= FAILURE_RETRY_DELAY,
        "expected a wake-up near the end of the retry delay, got {sleep_for:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_healthy_connection_is_unaffected_by_the_retry_delay() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    healthy_connection_inner(&database).await
}

async fn healthy_connection_inner(pool: &PgPool) -> Result<()> {
    clear_connections(pool).await?;
    let workspace_id = workspace(pool).await?;
    connection(pool, workspace_id, "youtube", None).await?;

    let worker = worker(pool.clone())?;
    let due = worker
        .find_due_connections()
        .await
        .context("find due connections")?;
    ensure!(
        due.len() == 1 && due[0].platform == "youtube",
        "a connection that has never failed must be due immediately; got {due:?}"
    );

    let next = worker
        .next_due_time()
        .await
        .context("a next due time must exist")?;
    let sleep_for = next.saturating_duration_since(tokio::time::Instant::now());
    ensure!(
        sleep_for < Duration::from_secs(5),
        "an unsynced healthy connection should wake the worker straight away, got {sleep_for:?}"
    );
    Ok(())
}
