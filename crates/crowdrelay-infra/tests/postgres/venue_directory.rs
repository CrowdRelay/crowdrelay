//! The resolution anchor, against a real schema.
//!
//! `place_venue_identifiers` is the load-bearing half of §12-4: one external
//! id must be one room forever, or every directory import re-splits the
//! evidence the registry exists to join. These tests drive the table
//! directly — the anchor's round-trip, the uniqueness rule across two
//! claimant venues, the coordinate-bearing mint, and the licence column the
//! ODbL facts carry.

use crate::common;

use crowdrelay_infra::venue_directory::PostgresVenueDirectoryRepository;
use crowdrelay_infra::venue_seed::{PostgresVenueSeedRepository, VenueFactWrite};
use sqlx::PgPool;
use uuid::Uuid;

async fn city_id(pool: &PgPool, slug: &str) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM cities WHERE slug = $1")
        .bind(slug)
        .fetch_one(pool)
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_anchor_is_one_room_whoever_claims_it() -> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run_anchor_cases(&database).await
}

async fn run_anchor_cases(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let directory = PostgresVenueDirectoryRepository::new(pool.clone());
    let wroclaw = city_id(pool, "wroclaw").await?;

    // ── Mint: a room nobody has played gets an identity row with the pin
    // the directory saw. ─────────────────────────────────────────────────
    let venue = directory
        .upsert_venue(wroclaw, "Klub Terminal", Some(51.094), Some(17.020))
        .await?;
    let (lat, lon): (Option<f64>, Option<f64>) =
        sqlx::query_as("SELECT latitude, longitude FROM place_venues WHERE id = $1")
            .bind(venue)
            .fetch_one(pool)
            .await?;
    assert_eq!(lat, Some(51.094));
    assert_eq!(lon, Some(17.020));

    // ── Link: the anchor resolves back to the room. ──────────────────────
    let bound = directory
        .link_venue_identifier(venue, "osm_node", "302516611")
        .await?;
    assert_eq!(bound, venue);
    assert_eq!(
        directory
            .resolve_identifier("osm_node", "302516611")
            .await?,
        Some(venue)
    );

    // ── The PK is the rule: a second venue cannot claim the same
    // (scheme, identifier). Linking it returns the original room — the
    // anchor wins over the newest claimant, and no second row appears.
    let other = directory
        .upsert_venue(wroclaw, "Terminal (duplicate spelling)", None, None)
        .await?;
    let bound = directory
        .link_venue_identifier(other, "osm_node", "302516611")
        .await?;
    assert_eq!(
        bound, venue,
        "the anchor must keep pointing at the first room, not the newest claimant"
    );
    let anchor_rows: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM place_venue_identifiers \
         WHERE scheme = 'osm_node' AND identifier = '302516611'",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(anchor_rows, 1);

    // ── Schemes are independent namespaces: the same digits under a
    // different scheme is a different anchor.
    let other_scheme = directory
        .link_venue_identifier(venue, "musicbrainz", "302516611")
        .await?;
    assert_eq!(other_scheme, venue);

    // ── A re-sweep updates the pin and keeps the room — no twin row. ─────
    let again = directory
        .upsert_venue(wroclaw, "Klub Terminal", Some(51.095), Some(17.021))
        .await?;
    assert_eq!(
        again, venue,
        "a re-sweep under the same name must not mint a twin"
    );
    let rooms: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM place_venues WHERE city_id = $1 AND name_key = place_venue_key('Klub Terminal')",
    )
    .bind(wroclaw)
    .fetch_one(pool)
    .await?;
    assert_eq!(rooms, 1);
    let (lat, lon): (Option<f64>, Option<f64>) =
        sqlx::query_as("SELECT latitude, longitude FROM place_venues WHERE id = $1")
            .bind(venue)
            .fetch_one(pool)
            .await?;
    assert_eq!((lat, lon), (Some(51.095), Some(17.021)));

    // ── The licence column takes 'odbl' and rejects anything else — the
    // CHECK is what keeps "which facts are ODbL-derived" answerable. ──────
    PostgresVenueSeedRepository::write_fact(
        pool,
        VenueFactWrite {
            venue_id: venue,
            attribute: "website",
            value: "https://terminal.example/",
            provenance: "open_directory",
            source_ref: "osm:node:302516611",
            observed_at: None,
            expires_at: None,
            workspace_id: None,
            licence: Some("odbl"),
        },
    )
    .await?;
    let licence: Option<String> = sqlx::query_scalar(
        "SELECT licence FROM place_venue_facts \
         WHERE venue_id = $1 AND attribute = 'website'",
    )
    .bind(venue)
    .fetch_one(pool)
    .await?;
    assert_eq!(licence.as_deref(), Some("odbl"));

    let rejected = PostgresVenueSeedRepository::write_fact(
        pool,
        VenueFactWrite {
            venue_id: venue,
            attribute: "capacity",
            value: "300",
            provenance: "open_directory",
            source_ref: "osm:node:302516611",
            observed_at: None,
            expires_at: None,
            workspace_id: None,
            licence: Some("cc0"),
        },
    )
    .await;
    assert!(
        rejected.is_err(),
        "a licence the CHECK does not name must fail"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_event_title_in_the_venue_field_mints_no_phantom_room()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let wroclaw = city_id(&pool, "wroclaw").await?;

    // A provider listing with no venue leaves the tour/show title in the
    // venue field. The trigger must never mint a room out of it — production
    // grew venues named "Sanity Check Tour" that way.
    let workspace = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Title Guard')")
        .bind(workspace)
        .bind(common::unique_slug("title-guard", workspace))
        .execute(&pool)
        .await?;

    let phantom_event = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events
            (id, workspace_id, city_id, slug, title, venue, starts_at, status, published_at)
         VALUES ($1, $2, $3, $4, 'Phantom Tour Night', 'phantom tour night',
                 now() + interval '20 days', 'published', now())",
    )
    .bind(phantom_event)
    .bind(workspace)
    .bind(wroclaw)
    .bind(common::unique_slug("phantom", phantom_event))
    .execute(&pool)
    .await?;

    let minted: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM place_venues WHERE city_id = $1 AND name_key = 'phantom tour night'",
    )
    .bind(wroclaw)
    .fetch_optional(&pool)
    .await?;
    assert!(minted.is_none(), "the event title is not a room");
    let mark: Option<Uuid> =
        sqlx::query_scalar("SELECT venue_id FROM place_venue_marks WHERE event_id = $1")
            .bind(phantom_event)
            .fetch_optional(&pool)
            .await?;
    assert!(mark.is_none(), "nothing to mark: no room was found");

    // The guard refuses only the mint: a room that genuinely bears the name
    // — a show literally named after its venue — still links to the row.
    let real = PostgresVenueDirectoryRepository::new(pool.clone())
        .upsert_venue(wroclaw, "Klub Echo", None, None)
        .await?;
    let named_event = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events
            (id, workspace_id, city_id, slug, title, venue, starts_at, status, published_at)
         VALUES ($1, $2, $3, $4, 'Klub Echo', 'klub  echo',
                 now() + interval '20 days', 'published', now())",
    )
    .bind(named_event)
    .bind(workspace)
    .bind(wroclaw)
    .bind(common::unique_slug("named", named_event))
    .execute(&pool)
    .await?;
    let linked: Option<Uuid> =
        sqlx::query_scalar("SELECT venue_id FROM place_venue_marks WHERE event_id = $1")
            .bind(named_event)
            .fetch_optional(&pool)
            .await?;
    assert_eq!(
        linked,
        Some(real),
        "the existing room still claims the mark"
    );

    // And an UPDATE that turns the venue field into the title retracts a
    // mark it previously made.
    sqlx::query("UPDATE events SET venue = 'Klub Echo' WHERE id = $1")
        .bind(phantom_event)
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE events SET venue = 'Phantom Tour Night' WHERE id = $1")
        .bind(phantom_event)
        .execute(&pool)
        .await?;
    let mark_after: Option<Uuid> =
        sqlx::query_scalar("SELECT venue_id FROM place_venue_marks WHERE event_id = $1")
            .bind(phantom_event)
            .fetch_optional(&pool)
            .await?;
    assert!(
        mark_after.is_none(),
        "flipping venue back to the title retracts the mark"
    );

    Ok(())
}
