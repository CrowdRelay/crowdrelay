//! The room a tenant played and the room a tenant pitches must resolve to one
//! `place_venues` row, in both directions and at any order of arrival.
//!
//! Migration 0290 built the shared registry and migration 0033 built the
//! booking pipeline, and nothing joined them: a band could hold fourteen marks
//! on Klub X and a booking target called "Klub X" with no way to notice they
//! were the same place. Every aggregate the registry computes was therefore
//! invisible to the pitch that needed it.
//!
//! Migration 0296 is that join, and it is entirely trigger-maintained, so the
//! only way to know it holds is to drive the tables. Each case below is a way
//! the link was wrong at some point while the migration was being written:
//! a target inserted before its room existed, a room created after its target,
//! a rename that should retract, and a promoter that must never acquire one.

use sqlx::{Connection, PgConnection, PgPool, Row, postgres::PgPoolOptions};
use uuid::Uuid;

struct DisposableDatabase {
    pool: PgPool,
    admin_url: String,
    name: String,
}

impl DisposableDatabase {
    async fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let base = std::env::var("CROWDRELAY_TEST_DATABASE_URL")
            .map_err(|_| "CROWDRELAY_TEST_DATABASE_URL must target a disposable database")?;
        let name = format!("crowdrelay_venuelink_{}", Uuid::now_v7().simple());
        let mut admin = PgConnection::connect(&base).await?;
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await?;
        drop(admin);
        let (head, _) = base.rsplit_once('/').ok_or("database url has no path")?;
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&format!("{head}/{name}"))
            .await?;
        crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
        Ok(Self {
            pool,
            admin_url: base,
            name,
        })
    }

    async fn drop_database(self) {
        let Self {
            pool,
            admin_url,
            name,
        } = self;
        pool.close().await;
        if let Ok(mut admin) = PgConnection::connect(&admin_url).await {
            let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                .execute(&mut admin)
                .await;
        }
    }
}

async fn seed_workspace(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("ws-{}", id.simple()))
        .bind("Test Workspace")
        .execute(pool)
        .await?;
    Ok(id)
}

/// Resolves a city, creating it only when the migrations did not.
///
/// Two things this got wrong first. Cities are unique on
/// `(country_code, slug)` and not on slug alone — two countries may each hold
/// a "praha" — so that is the conflict target. And a disposable database is
/// not empty: the migrations seed a city catalogue, so `wroclaw` and `praha`
/// already exist by the time a test runs.
async fn seed_city(pool: &PgPool, slug: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO cities (slug, name, country_code) VALUES ($1, $2, 'PL')
         ON CONFLICT (country_code, slug) DO NOTHING",
    )
    .bind(slug)
    .bind(slug)
    .execute(pool)
    .await?;
    let id = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM cities WHERE country_code = 'PL' AND slug = $1",
    )
    .bind(slug)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// A room in the registry, created the way the event trigger would.
async fn seed_venue(
    pool: &PgPool,
    city_id: Uuid,
    display_name: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO place_venues (city_id, name_key, display_name)
         VALUES ($1, place_venue_key($2), $2) RETURNING id",
    )
    .bind(city_id)
    .bind(display_name)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

async fn seed_target(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    kind: &str,
    display_name: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO viryaos_booking_targets
             (workspace_id, city_id, target_kind, display_name, contact_email)
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(kind)
    .bind(display_name)
    .bind(format!("booking+{}@example.com", Uuid::now_v7().simple()))
    .fetch_one(pool)
    .await?;
    Ok(id)
}

async fn venue_link(pool: &PgPool, target_id: Uuid) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT venue_id FROM viryaos_booking_targets WHERE id = $1",
    )
    .bind(target_id)
    .fetch_one(pool)
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_booking_target_resolves_to_the_room_it_names() -> Result<(), Box<dyn std::error::Error>>
{
    let database = DisposableDatabase::create().await?;
    let result = run_cases(&database.pool).await;
    database.drop_database().await;
    result
}

async fn run_cases(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = seed_workspace(pool).await?;
    let wroclaw = seed_city(pool, "wroclaw").await?;
    let praha = seed_city(pool, "praha").await?;

    // ── The room exists first: a target naming it links on insert. ──────────
    let klub_x = seed_venue(pool, wroclaw, "Klub X").await?;
    let target = seed_target(pool, workspace, wroclaw, "venue", "Klub X").await?;
    assert_eq!(
        venue_link(pool, target).await?,
        Some(klub_x),
        "a target naming a known room did not link to it"
    );

    // Casing and doubled whitespace are the same room. This is the whole
    // reason the trigger calls `place_venue_key` rather than comparing text:
    // two answers to "is this the same room" is the shape of every duplicate
    // in this codebase.
    let sloppy = seed_target(pool, workspace, wroclaw, "venue", "  klub    X ").await?;
    assert_eq!(
        venue_link(pool, sloppy).await?,
        Some(klub_x),
        "casing and whitespace produced a second answer"
    );

    // ── Same name, different city: two rooms, and the link follows the city.
    let klub_x_praha = seed_venue(pool, praha, "Klub X").await?;
    assert_ne!(klub_x, klub_x_praha);
    let praha_target = seed_target(pool, workspace, praha, "venue", "Klub X").await?;
    assert_eq!(
        venue_link(pool, praha_target).await?,
        Some(klub_x_praha),
        "a target linked to a room of the same name in another city"
    );

    // ── The target exists first: a room arriving later links it. ────────────
    //
    // Without the AFTER INSERT trigger on `place_venues`, a band that pitched
    // a room in March and first played it in June would still show no history,
    // because its target row never changed.
    let unknown = seed_target(pool, workspace, wroclaw, "venue", "Klub Y").await?;
    assert_eq!(
        venue_link(pool, unknown).await?,
        None,
        "a room nobody has played must be an absent link, never a guess"
    );
    let klub_y = seed_venue(pool, wroclaw, "KLUB Y").await?;
    assert_eq!(
        venue_link(pool, unknown).await?,
        Some(klub_y),
        "a room entering the registry did not claim the target already naming it"
    );

    // ── A rename out of a match retracts, rather than keeping a stale claim.
    sqlx::query("UPDATE viryaos_booking_targets SET display_name = 'Klub Z' WHERE id = $1")
        .bind(unknown)
        .execute(pool)
        .await?;
    assert_eq!(
        venue_link(pool, unknown).await?,
        None,
        "a target renamed away from its room kept the old link"
    );

    // ── A promoter is not a room. ───────────────────────────────────────────
    //
    // A promoter books rooms and a festival is an event. Matching either by
    // name would attach a room's show history to something that is not a room,
    // and the resulting number would be nonsense nobody could trace back.
    let promoter = seed_target(pool, workspace, wroclaw, "promoter", "Klub X").await?;
    assert_eq!(
        venue_link(pool, promoter).await?,
        None,
        "a promoter sharing a room's name acquired the room's identity"
    );

    // And the CHECK holds even against a direct write that bypasses the
    // trigger's intent — the constraint is the guarantee, the trigger is the
    // convenience.
    let forced = sqlx::query("UPDATE viryaos_booking_targets SET venue_id = $2 WHERE id = $1")
        .bind(promoter)
        .bind(klub_x)
        .execute(pool)
        .await;
    assert!(
        forced.is_err(),
        "the CHECK allowed a non-venue target to hold a venue link"
    );

    // ── A venue leaving the registry drops the link, never the relationship.
    //
    // The room record is shared; the contact and its history belong to the
    // tenant, and deleting a shared row must not take them.
    sqlx::query("DELETE FROM place_venues WHERE id = $1")
        .bind(klub_x)
        .execute(pool)
        .await?;
    let surviving =
        sqlx::query("SELECT venue_id, contact_email FROM viryaos_booking_targets WHERE id = $1")
            .bind(target)
            .fetch_optional(pool)
            .await?
            .ok_or("the target was deleted along with the room")?;
    assert_eq!(surviving.get::<Option<Uuid>, _>("venue_id"), None);
    assert!(!surviving.get::<String, _>("contact_email").is_empty());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_backfill_links_targets_that_predate_the_migration()
-> Result<(), Box<dyn std::error::Error>> {
    // The backfill is the half of 0296 the triggers cannot cover: rows that
    // existed before it ran. Simulated by dropping the trigger, writing the
    // rows the way the old schema would have, and running the backfill's own
    // statement — the alternative is asserting nothing about it, which is how
    // a backfill ships broken.
    let database = DisposableDatabase::create().await?;
    let result = async {
        let pool = &database.pool;
        let workspace = seed_workspace(pool).await?;
        let wroclaw = seed_city(pool, "wroclaw").await?;

        sqlx::query(
            "DROP TRIGGER viryaos_booking_targets_resolve_venue ON viryaos_booking_targets",
        )
        .execute(pool)
        .await?;
        let target = seed_target(pool, workspace, wroclaw, "venue", "Klub X").await?;
        let klub_x = seed_venue(pool, wroclaw, "klub x").await?;
        // The place_venues trigger is still live, so clear its work to model a
        // pair of rows that genuinely never met.
        sqlx::query("UPDATE viryaos_booking_targets SET venue_id = NULL WHERE id = $1")
            .bind(target)
            .execute(pool)
            .await?;
        assert_eq!(
            venue_link(pool, target).await?,
            None,
            "fixture is not unlinked"
        );

        sqlx::query(
            r#"
            UPDATE viryaos_booking_targets AS target
            SET venue_id = venue.id
            FROM place_venues AS venue
            WHERE target.target_kind = 'venue'
              AND venue.city_id = target.city_id
              AND venue.name_key = place_venue_key(target.display_name)
              AND target.venue_id IS NULL
            "#,
        )
        .execute(pool)
        .await?;
        assert_eq!(
            venue_link(pool, target).await?,
            Some(klub_x),
            "the backfill left a resolvable pair unlinked"
        );
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    database.drop_database().await;
    result
}
