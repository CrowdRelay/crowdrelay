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

use crate::common;

use sqlx::{PgConnection, Row};
use uuid::Uuid;

async fn seed_workspace(conn: &mut PgConnection) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("ws-{}", id.simple()))
        .bind("Test Workspace")
        .execute(&mut *conn)
        .await?;
    Ok(id)
}

/// A city of this run's own.
///
/// `place_venues` is shared across tenants and unique on `(city_id,
/// name_key)`, and the suite shares one database. Pinning "Klub X" to the
/// migration-seeded `wroclaw` made the second run of this file collide with
/// the first run's room, so each run takes a fresh city.
async fn seed_city(
    conn: &mut PgConnection,
    name: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    // Named by its slug too: a second PL city called "wroclaw" would make the
    // real one ambiguous to every by-name lookup.
    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO cities (slug, name, country_code) VALUES ($1, $1, 'PL') RETURNING id",
    )
    .bind(common::unique_slug(name, Uuid::now_v7()))
    .fetch_one(&mut *conn)
    .await?;
    Ok(id)
}

/// A room in the registry, created the way the event trigger would.
async fn seed_venue(
    conn: &mut PgConnection,
    city_id: Uuid,
    display_name: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO place_venues (city_id, name_key, display_name)
         VALUES ($1, place_venue_key($2), $2) RETURNING id",
    )
    .bind(city_id)
    .bind(display_name)
    .fetch_one(&mut *conn)
    .await?;
    Ok(id)
}

async fn seed_target(
    conn: &mut PgConnection,
    workspace_id: Uuid,
    city_id: Uuid,
    kind: &str,
    display_name: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO booking_targets
             (workspace_id, city_id, target_kind, display_name, contact_email)
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(kind)
    .bind(display_name)
    .bind(format!("booking+{}@example.com", Uuid::now_v7().simple()))
    .fetch_one(&mut *conn)
    .await?;
    Ok(id)
}

async fn venue_link(conn: &mut PgConnection, target_id: Uuid) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<Uuid>>("SELECT venue_id FROM booking_targets WHERE id = $1")
        .bind(target_id)
        .fetch_one(&mut *conn)
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_booking_target_resolves_to_the_room_it_names() -> Result<(), Box<dyn std::error::Error>>
{
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    let mut conn = database.acquire().await?;
    run_cases(&mut conn).await
}

async fn run_cases(conn: &mut PgConnection) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = seed_workspace(conn).await?;
    let wroclaw = seed_city(conn, "wroclaw").await?;
    let praha = seed_city(conn, "praha").await?;

    // ── The room exists first: a target naming it links on insert. ──────────
    let klub_x = seed_venue(conn, wroclaw, "Klub X").await?;
    let target = seed_target(conn, workspace, wroclaw, "venue", "Klub X").await?;
    assert_eq!(
        venue_link(conn, target).await?,
        Some(klub_x),
        "a target naming a known room did not link to it"
    );

    // Casing and doubled whitespace are the same room. This is the whole
    // reason the trigger calls `place_venue_key` rather than comparing text:
    // two answers to "is this the same room" is the shape of every duplicate
    // in this codebase.
    let sloppy = seed_target(conn, workspace, wroclaw, "venue", "  klub    X ").await?;
    assert_eq!(
        venue_link(conn, sloppy).await?,
        Some(klub_x),
        "casing and whitespace produced a second answer"
    );

    // ── Same name, different city: two rooms, and the link follows the city.
    let klub_x_praha = seed_venue(conn, praha, "Klub X").await?;
    assert_ne!(klub_x, klub_x_praha);
    let praha_target = seed_target(conn, workspace, praha, "venue", "Klub X").await?;
    assert_eq!(
        venue_link(conn, praha_target).await?,
        Some(klub_x_praha),
        "a target linked to a room of the same name in another city"
    );

    // ── The target exists first: a room arriving later links it. ────────────
    //
    // Without the AFTER INSERT trigger on `place_venues`, a band that pitched
    // a room in March and first played it in June would still show no history,
    // because its target row never changed.
    let unknown = seed_target(conn, workspace, wroclaw, "venue", "Klub Y").await?;
    assert_eq!(
        venue_link(conn, unknown).await?,
        None,
        "a room nobody has played must be an absent link, never a guess"
    );
    let klub_y = seed_venue(conn, wroclaw, "KLUB Y").await?;
    assert_eq!(
        venue_link(conn, unknown).await?,
        Some(klub_y),
        "a room entering the registry did not claim the target already naming it"
    );

    // ── A rename out of a match retracts, rather than keeping a stale claim.
    sqlx::query("UPDATE booking_targets SET display_name = 'Klub Z' WHERE id = $1")
        .bind(unknown)
        .execute(&mut *conn)
        .await?;
    assert_eq!(
        venue_link(conn, unknown).await?,
        None,
        "a target renamed away from its room kept the old link"
    );

    // ── A promoter is not a room. ───────────────────────────────────────────
    //
    // A promoter books rooms and a festival is an event. Matching either by
    // name would attach a room's show history to something that is not a room,
    // and the resulting number would be nonsense nobody could trace back.
    let promoter = seed_target(conn, workspace, wroclaw, "promoter", "Klub X").await?;
    assert_eq!(
        venue_link(conn, promoter).await?,
        None,
        "a promoter sharing a room's name acquired the room's identity"
    );

    // And the CHECK holds even against a direct write that bypasses the
    // trigger's intent — the constraint is the guarantee, the trigger is the
    // convenience.
    let forced = sqlx::query("UPDATE booking_targets SET venue_id = $2 WHERE id = $1")
        .bind(promoter)
        .bind(klub_x)
        .execute(&mut *conn)
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
        .execute(&mut *conn)
        .await?;
    let surviving =
        sqlx::query("SELECT venue_id, contact_email FROM booking_targets WHERE id = $1")
            .bind(target)
            .fetch_optional(&mut *conn)
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
    //
    // The trigger drop is DDL on the one database the whole suite shares, so
    // it runs inside a transaction that is always rolled back: committed, it
    // silently unlinked every booking target any later test wrote.
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let mut tx = database.begin().await?;

    let outcome = async {
        let conn: &mut PgConnection = &mut tx;
        let workspace = seed_workspace(conn).await?;
        let wroclaw = seed_city(conn, "wroclaw").await?;

        sqlx::query("DROP TRIGGER booking_targets_resolve_venue ON booking_targets")
            .execute(&mut *conn)
            .await?;
        let target = seed_target(conn, workspace, wroclaw, "venue", "Klub X").await?;
        let klub_x = seed_venue(conn, wroclaw, "klub x").await?;
        // The place_venues trigger is still live, so clear its work to model a
        // pair of rows that genuinely never met.
        sqlx::query("UPDATE booking_targets SET venue_id = NULL WHERE id = $1")
            .bind(target)
            .execute(&mut *conn)
            .await?;
        assert_eq!(
            venue_link(conn, target).await?,
            None,
            "fixture is not unlinked"
        );

        sqlx::query(
            r#"
            UPDATE booking_targets AS target
            SET venue_id = venue.id
            FROM place_venues AS venue
            WHERE target.target_kind = 'venue'
              AND venue.city_id = target.city_id
              AND venue.name_key = place_venue_key(target.display_name)
              AND target.venue_id IS NULL
            "#,
        )
        .execute(&mut *conn)
        .await?;
        assert_eq!(
            venue_link(conn, target).await?,
            Some(klub_x),
            "the backfill left a resolvable pair unlinked"
        );
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    tx.rollback().await?;
    outcome
}
