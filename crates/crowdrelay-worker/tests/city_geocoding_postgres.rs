//! The geocoding worker's database state machine, against a real schema.
//!
//! The unit tests cover how a provider reply is read. What matters here is what
//! the worker does with it: a resolved city must never be looked up again, a
//! failed one must back off rather than retry every poll, and a name nobody can
//! resolve must eventually stop costing requests instead of spinning forever.
//! None of that is observable without the row.
//!
//! `cities` is a shared catalogue with no workspace column, so the worker's
//! selection is global by design and any unresolved city left behind by another
//! suite would land in the same batch. This test therefore creates and drops its
//! own database rather than sharing one: batch counts only mean something when
//! the batch is exactly what the test put there.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use crowdrelay_worker::city_geocoding::{
    CityGeocodeWorker, GeocodeProvider, GeocodedPoint, MAX_GEOCODE_ATTEMPTS,
};
use sqlx::{Connection, PgConnection, PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

/// Replies from a fixed script and counts how often it was asked. The count is
/// the point: "did not ask again" is the cache assertion.
struct ScriptedProvider {
    answer: Option<GeocodedPoint>,
    fail: bool,
    calls: AtomicUsize,
}

impl ScriptedProvider {
    fn new(answer: Option<GeocodedPoint>, fail: bool) -> Arc<Self> {
        Arc::new(Self {
            answer,
            fail,
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl GeocodeProvider for ScriptedProvider {
    async fn lookup(
        &self,
        _name: &str,
        _region: Option<&str>,
        _country_code: &str,
    ) -> Result<Option<GeocodedPoint>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            anyhow::bail!("provider unavailable");
        }
        Ok(self.answer)
    }
}

/// Splits `postgres://.../name` into the same URL pointed at `postgres` and the
/// database name, so the test can create and drop its own.
fn split_database_url(url: &str) -> Result<(String, String)> {
    let (prefix, name) = url
        .rsplit_once('/')
        .context("database URL has no database name")?;
    let name = name.split('?').next().unwrap_or(name);
    ensure!(!name.is_empty(), "database URL has an empty database name");
    Ok((format!("{prefix}/postgres"), name.to_owned()))
}

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
        let name = format!("crowdrelay_geocode_{}", Uuid::now_v7().simple());
        let mut admin = PgConnection::connect(&admin_url)
            .await
            .context("connect to the maintenance database")?;
        // The name is a fresh UUID, so there is nothing to quote-escape and no
        // caller-supplied text reaches this statement.
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await
            .context("create the disposable database")?;
        drop(admin);

        let (prefix, _) = base_url
            .rsplit_once('/')
            .context("database URL has no database name")?;
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&format!("{prefix}/{name}"))
            .await
            .context("connect to the disposable database")?;
        crowdrelay_infra::database::MIGRATOR
            .run(&pool)
            .await
            .context("apply migrations")?;
        Ok(Self {
            admin_url,
            name,
            pool,
        })
    }

    async fn drop_database(self) {
        self.pool.close().await;
        if let Ok(mut admin) = PgConnection::connect(&self.admin_url).await {
            let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {} (FORCE)", self.name))
                .execute(&mut admin)
                .await;
        }
    }
}

async fn insert_pending_city(pool: &PgPool, slug: &str) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO cities (id, slug, name, country_code, region, moderation_status, request_count)
        VALUES ($1, $2, $3, 'PL', 'Dolnoslaskie', 'pending', 3)
        "#,
    )
    .bind(id)
    .bind(slug)
    .bind(format!("Geocode Test {slug}"))
    .execute(pool)
    .await
    .context("insert pending city")?;
    Ok(id)
}

/// `(latitude, attempts, last_error, is_backing_off)`.
async fn city_state(pool: &PgPool, id: Uuid) -> Result<(Option<f64>, i32, Option<String>, bool)> {
    sqlx::query_as(
        r#"
        SELECT latitude,
               geocode_attempts,
               geocode_last_error,
               (geocode_next_attempt_at IS NOT NULL AND geocode_next_attempt_at > now())
        FROM cities
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .context("read city state")
}

/// Removes the cities the bootstrap migrations seed, so a batch contains only
/// what the scenario inserted.
async fn clear_catalogue(pool: &PgPool) -> Result<()> {
    sqlx::query("DELETE FROM cities")
        .execute(pool)
        .await
        .context("clear the seeded catalogue")?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_geocoding_state_machine_resolves_backs_off_and_gives_up() -> Result<()> {
    let database = DisposableDatabase::create().await?;
    let result = run_scenarios(&database.pool).await;
    database.drop_database().await;
    result
}

async fn run_scenarios(pool: &PgPool) -> Result<()> {
    clear_catalogue(pool).await?;
    resolved_city_is_stored_once_and_never_looked_up_again(pool).await?;

    clear_catalogue(pool).await?;
    a_failure_backs_off_and_then_stops_at_the_cap(pool).await?;

    clear_catalogue(pool).await?;
    a_name_with_no_match_is_recorded_as_such(pool).await?;

    clear_catalogue(pool).await?;
    a_requested_city_becomes_a_queued_push(pool).await
}

/// The whole retention loop in one pass: a fan names a city nobody has
/// coordinates for, the geocoding worker supplies them, and the nearby-show
/// emitter turns that into a notification and a queued push.
///
/// Every one of those steps has been present and inert at some point. Asserting
/// them separately never caught it, because each piece worked and nothing joined
/// them up.
async fn a_requested_city_becomes_a_queued_push(pool: &PgPool) -> Result<()> {
    let suffix = Uuid::now_v7().simple().to_string();
    let workspace_id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Retention loop test')")
        .bind(workspace_id)
        .bind(format!("loop-{suffix}"))
        .execute(pool)
        .await
        .context("insert workspace")?;

    let fan_city = insert_pending_city(pool, &format!("loop-home-{suffix}")).await?;
    let show_city = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ($1, 'Show city', 'PL', 51.4, 17.0) RETURNING id",
    )
    .bind(format!("loop-show-{suffix}"))
    .fetch_one(pool)
    .await
    .context("insert show city")?;

    let fan_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO fans (workspace_id, normalized_email, locale, status)
         VALUES ($1, $2, 'pl-PL', 'active') RETURNING id",
    )
    .bind(workspace_id)
    .bind(format!("loop-{suffix}@example.test"))
    .fetch_one(pool)
    .await
    .context("insert fan")?;
    // Buying a ticket makes an active fan and grants nothing; the loop needs
    // current marketing consent on top of that, for mail and push alike.
    sqlx::query(
        "INSERT INTO fan_consents
             (workspace_id, fan_id, purpose, granted, policy_version, source, request_id)
         VALUES ($1, $2, 'marketing', true, 'privacy-v1', 'test', $3)",
    )
    .bind(workspace_id)
    .bind(fan_id)
    .bind(format!("consent-{suffix}"))
    .execute(pool)
    .await
    .context("record consent")?;
    sqlx::query(
        "INSERT INTO fan_location_preferences
             (workspace_id, fan_id, city_id, nearby_gigs_enabled, radius_km)
         VALUES ($1, $2, $3, true, 150)",
    )
    .bind(workspace_id)
    .bind(fan_id)
    .bind(fan_city)
    .execute(pool)
    .await
    .context("set the fan location preference")?;
    sqlx::query(
        "INSERT INTO fan_push_endpoints
             (workspace_id, fan_id, installation_id, transport, endpoint_address)
         VALUES ($1, $2, $3, 'android_fcm', $4)",
    )
    .bind(workspace_id)
    .bind(fan_id)
    .bind(format!("install-{suffix}"))
    .bind(format!("fcm-token-{suffix}-0123456789abcdef"))
    .execute(pool)
    .await
    .context("register a push endpoint")?;
    sqlx::query(
        "INSERT INTO events (workspace_id, city_id, slug, title, starts_at, status, published_at)
         VALUES ($1, $2, $3, 'Show', now() + interval '30 days', 'published', now())",
    )
    .bind(workspace_id)
    .bind(show_city)
    .bind(format!("loop-show-{suffix}"))
    .execute(pool)
    .await
    .context("publish a show")?;

    let repository = crowdrelay_infra::mobile_fan::PostgresMobileFanRepository::new(
        pool.clone(),
        crowdrelay_domain::WorkspaceId::from_uuid(workspace_id),
        Duration::from_secs(5),
    );

    // Before coordinates the fan is unreachable however willing they are. This
    // is the state the queue sat in.
    let (mailed, pushed) = repository
        .emit_due_nearby_gigs(Some(&format!("before-{suffix}")), true)
        .await
        .map_err(|error| anyhow::anyhow!("emit before geocoding: {error:?}"))?;
    ensure!(
        (mailed, pushed) == (0, 0),
        "a city without coordinates must reach nobody, got ({mailed}, {pushed})"
    );

    let provider = ScriptedProvider::new(
        Some(GeocodedPoint {
            latitude: 51.1079,
            longitude: 17.0385,
        }),
        false,
    );
    let (resolved, failed) =
        CityGeocodeWorker::new(pool.clone(), provider, Duration::from_secs(60 * 60))
            .resolve_batch()
            .await?;
    ensure!(
        (resolved, failed) == (1, 0),
        "the worker should have supplied the missing coordinates"
    );

    let (mailed, pushed) = repository
        .emit_due_nearby_gigs(Some(&format!("after-{suffix}")), true)
        .await
        .map_err(|error| anyhow::anyhow!("emit after geocoding: {error:?}"))?;
    ensure!(
        (mailed, pushed) == (1, 1),
        "the fan should get one mail intent and one queued push, got ({mailed}, {pushed})"
    );

    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM fan_push_deliveries
         WHERE workspace_id = $1 AND status = 'queued' AND source_kind = 'nearby_concert'",
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await
    .context("count queued pushes")?;
    ensure!(queued == 1, "the push must be durable, not just counted");

    // Re-running must not announce the same show twice: the loop is scheduled,
    // so a double run has to be free.
    let (mailed, pushed) = repository
        .emit_due_nearby_gigs(Some(&format!("again-{suffix}")), true)
        .await
        .map_err(|error| anyhow::anyhow!("emit again: {error:?}"))?;
    ensure!(
        (mailed, pushed) == (0, 0),
        "a second pass must not repeat an announcement"
    );

    // The moderation queue is a separate concern from reachability: this city
    // is still awaiting a human and is already reaching its fans.
    let still_pending: String =
        sqlx::query_scalar("SELECT moderation_status FROM cities WHERE id = $1")
            .bind(fan_city)
            .fetch_one(pool)
            .await
            .context("read moderation status")?;
    ensure!(
        still_pending == "pending",
        "geocoding must not approve a city on a human's behalf"
    );
    Ok(())
}

async fn resolved_city_is_stored_once_and_never_looked_up_again(pool: &PgPool) -> Result<()> {
    let city_id = insert_pending_city(pool, "geocode-resolves").await?;
    let provider = ScriptedProvider::new(
        Some(GeocodedPoint {
            latitude: 51.1079,
            longitude: 17.0385,
        }),
        false,
    );
    let worker =
        CityGeocodeWorker::new(pool.clone(), provider.clone(), Duration::from_secs(60 * 60));

    let (resolved, failed) = worker.resolve_batch().await?;
    ensure!(
        (resolved, failed) == (1, 0),
        "first pass should resolve the city, got ({resolved}, {failed})"
    );
    let (latitude, attempts, error, backing_off) = city_state(pool, city_id).await?;
    ensure!(latitude == Some(51.1079), "coordinates must be stored");
    ensure!(attempts == 0, "a success must not count as an attempt");
    ensure!(error.is_none(), "a success must clear the last error");
    ensure!(!backing_off, "a resolved city has no next attempt");

    // The row is the cache: a second pass must not reach the provider at all.
    // Without this the worker would re-ask for every city it already knows, on
    // every poll, forever.
    let (resolved, failed) = worker.resolve_batch().await?;
    ensure!(
        (resolved, failed) == (0, 0),
        "a resolved city must not be selected again"
    );
    ensure!(
        provider.calls() == 1,
        "provider was asked {} times; expected exactly one",
        provider.calls()
    );
    Ok(())
}

async fn a_failure_backs_off_and_then_stops_at_the_cap(pool: &PgPool) -> Result<()> {
    let city_id = insert_pending_city(pool, "geocode-fails").await?;
    let provider = ScriptedProvider::new(None, true);
    let worker =
        CityGeocodeWorker::new(pool.clone(), provider.clone(), Duration::from_secs(60 * 60));

    let (resolved, failed) = worker.resolve_batch().await?;
    ensure!(
        (resolved, failed) == (0, 1),
        "a provider failure counts as a failed city"
    );
    let (latitude, attempts, error, backing_off) = city_state(pool, city_id).await?;
    ensure!(latitude.is_none(), "a failure must not invent coordinates");
    ensure!(attempts == 1, "the attempt must be recorded");
    ensure!(error.is_some(), "the reason must be kept for the operator");
    ensure!(
        backing_off,
        "a failed city must wait before it is retried, not retry on the next poll"
    );

    let (_, failed) = worker.resolve_batch().await?;
    ensure!(failed == 0, "the backoff must hold off the next pass");
    ensure!(provider.calls() == 1, "the provider must not be re-asked");

    // Wind the counter to the cap: a name nobody can resolve has to leave the
    // queue for good rather than cost a request every backoff window.
    sqlx::query(
        "UPDATE cities SET geocode_attempts = $2, geocode_next_attempt_at = NULL WHERE id = $1",
    )
    .bind(city_id)
    .bind(MAX_GEOCODE_ATTEMPTS)
    .execute(pool)
    .await
    .context("wind the attempt counter to the cap")?;
    let (resolved, failed) = worker.resolve_batch().await?;
    ensure!(
        (resolved, failed) == (0, 0),
        "a city at the attempt cap must not be selected"
    );
    ensure!(
        provider.calls() == 1,
        "a capped city must never reach the provider again"
    );
    Ok(())
}

async fn a_name_with_no_match_is_recorded_as_such(pool: &PgPool) -> Result<()> {
    let city_id = insert_pending_city(pool, "geocode-unknown").await?;
    let provider = ScriptedProvider::new(None, false);
    let worker =
        CityGeocodeWorker::new(pool.clone(), provider.clone(), Duration::from_secs(60 * 60));

    let (resolved, failed) = worker.resolve_batch().await?;
    ensure!(
        (resolved, failed) == (0, 1),
        "an unmatched name is a failure, not a success"
    );
    let (latitude, attempts, error, _) = city_state(pool, city_id).await?;
    ensure!(latitude.is_none(), "no match means no coordinates");
    ensure!(attempts == 1, "an unmatched name counts against the cap");
    ensure!(
        error.as_deref() == Some("no match for this name"),
        "the operator queue needs to see why, got {error:?}"
    );
    Ok(())
}
