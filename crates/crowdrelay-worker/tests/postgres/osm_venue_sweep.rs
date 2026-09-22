//! The OSM venue sweep against a real schema (§12-3, 4V.8).
//!
//! What matters is not the wire — that is stubbed — but what the sweep
//! leaves on disk: a room nobody has played minted with its coordinates and
//! its `osm_*` anchor, every fact carrying `open_directory` provenance and
//! the ODbL licence, a re-sweep refreshing rather than duplicating, and an
//! OSM `contact:email` tag producing no fact row at all — a scraped general
//! inbox is not a booking route.
//!
//! The same file drives the expiry sweep: an expired fact must be *gone*,
//! not filtered — the row is the thing the licence says we may not hold.

use crate::common;

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use async_trait::async_trait;
use crowdrelay_infra::{
    venue_directory::PostgresVenueDirectoryRepository,
    venue_seed::{PostgresVenueSeedRepository, VenueFactWrite},
};
use crowdrelay_worker::{
    osm_venue_sweep::{Bbox, OsmElement, OsmVenueSweepWorker, OverpassProvider},
    venue_fact_expiry::delete_expired_facts,
};
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// Answers every city with the same canned element list — the sweep's own
/// discipline (one call per city, spacing, per-element ingest) is what the
/// test exercises, not Overpass.
struct CannedProvider {
    elements: Vec<OsmElement>,
}

#[async_trait]
impl OverpassProvider for CannedProvider {
    async fn elements_in(&self, _bbox: Bbox) -> Result<Vec<OsmElement>> {
        Ok(self.elements.clone())
    }
}

fn element(id: u64, name: &str, tags: &[(&str, &str)]) -> OsmElement {
    let mut tags: HashMap<String, String> = tags
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    tags.insert("name".to_owned(), name.to_owned());
    OsmElement {
        kind: "node".to_owned(),
        id,
        lat: Some(51.094),
        lon: Some(17.020),
        center: None,
        tags,
    }
}

async fn seed_workspace(pool: &PgPool) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("ws-{}", id.simple()))
        .bind("Test Workspace")
        .execute(pool)
        .await?;
    Ok(id)
}

/// A city with a pin and one published show — the sweep's selection rule
/// made concrete.
async fn city_with_a_show(pool: &PgPool, workspace: Uuid, slug: &str) -> Result<Uuid> {
    let city_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ($1, 'Sweep City', 'PL', 51.1, 17.03) RETURNING id",
    )
    .bind(slug)
    .fetch_one(pool)
    .await
    .context("insert sweep city")?;
    sqlx::query(
        "INSERT INTO events (workspace_id, city_id, slug, title, starts_at, status)
         VALUES ($1, $2, $3, 'A show', now() - interval '10 days', 'completed')",
    )
    .bind(workspace)
    .bind(city_id)
    .bind(format!("show-{slug}"))
    .execute(pool)
    .await
    .context("insert the show that makes the city matter")?;
    Ok(city_id)
}

fn sweep(pool: &PgPool, elements: Vec<OsmElement>) -> OsmVenueSweepWorker {
    OsmVenueSweepWorker::new(
        pool.clone(),
        Arc::new(CannedProvider { elements }),
        Duration::from_secs(60),
        Duration::from_secs(10),
    )
}

async fn facts_for(
    pool: &PgPool,
    venue_id: Uuid,
) -> Result<Vec<(String, String, String, Option<String>, Option<Uuid>, String)>> {
    let rows = sqlx::query(
        "SELECT attribute, value, provenance, licence, workspace_id, source_ref \
         FROM place_venue_facts WHERE venue_id = $1 ORDER BY attribute",
    )
    .bind(venue_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|row| {
            (
                row.get::<String, _>("attribute"),
                row.get::<String, _>("value"),
                row.get::<String, _>("provenance"),
                row.get::<Option<String>, _>("licence"),
                row.get::<Option<Uuid>, _>("workspace_id"),
                row.get::<String, _>("source_ref"),
            )
        })
        .collect())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_swept_room_exists_with_its_anchor_and_licensed_facts() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run_sweep_cases(&database).await
}

async fn run_sweep_cases(pool: &PgPool) -> Result<()> {
    let workspace = seed_workspace(pool).await?;
    let city_id = city_with_a_show(pool, workspace, "sweep-city").await?;

    let worker = sweep(
        pool,
        vec![
            element(
                555_000_001,
                "Klub Firlej",
                &[
                    ("amenity", "nightclub"),
                    ("website", "https://firlej.example/"),
                    ("addr:street", "Grabiszyńska"),
                    ("addr:housenumber", "56"),
                    ("addr:city", "Wrocław"),
                    ("capacity", "250"),
                    // The general inbox OSM happens to carry — it must never
                    // become a row (BookingRefusalReason::RouteInferred).
                    ("contact:email", "info@firlej.example"),
                    ("email", "booking@firlej.example"),
                ],
            ),
            // A way with a center and no lat/lon of its own.
            OsmElement {
                kind: "way".to_owned(),
                id: 777_000_002,
                lat: None,
                lon: None,
                center: Some(crowdrelay_worker::osm_venue_sweep::OsmCenter {
                    lat: 51.111,
                    lon: 17.041,
                }),
                tags: HashMap::from([
                    ("amenity".to_owned(), "concert_hall".to_owned()),
                    ("name".to_owned(), "Sala Ziemi".to_owned()),
                ]),
            },
            // A nameless node — swept, counted, never minted.
            OsmElement {
                kind: "node".to_owned(),
                id: 999,
                lat: Some(51.1),
                lon: Some(17.0),
                center: None,
                tags: HashMap::from([("amenity".to_owned(), "nightclub".to_owned())]),
            },
        ],
    );

    let summary = worker.sweep_once().await?;
    assert_eq!(
        summary.cities, 1,
        "the seeded city was the sweep's only target"
    );
    assert_eq!(summary.written, 2);
    assert_eq!(
        summary.skipped, 1,
        "the nameless node was counted and dropped"
    );
    assert_eq!(summary.failed, 0);

    // ── The room exists with a location — the done-when, verbatim. ───────
    let directory = PostgresVenueDirectoryRepository::new(pool.clone());
    let venue = directory
        .resolve_identifier("osm_node", "555000001")
        .await?
        .context("the osm_node anchor did not resolve")?;
    let (name, lat, lon, venue_city): (String, Option<f64>, Option<f64>, Uuid) = sqlx::query_as(
        "SELECT display_name, latitude, longitude, city_id FROM place_venues WHERE id = $1",
    )
    .bind(venue)
    .fetch_one(pool)
    .await?;
    assert_eq!(name, "Klub Firlej");
    assert_eq!((lat, lon), (Some(51.094), Some(17.020)));
    assert_eq!(venue_city, city_id);

    // ── The way anchored through `osm_way` and took its pin from center. ──
    let way_venue = directory
        .resolve_identifier("osm_way", "777000002")
        .await?
        .context("the osm_way anchor did not resolve")?;
    let (lat, lon): (Option<f64>, Option<f64>) =
        sqlx::query_as("SELECT latitude, longitude FROM place_venues WHERE id = $1")
            .bind(way_venue)
            .fetch_one(pool)
            .await?;
    assert_eq!((lat, lon), (Some(51.111), Some(17.041)));

    // ── Every fact is open_directory + odbl + global + the osm source. ────
    let facts = facts_for(pool, venue).await?;
    assert_eq!(
        facts.len(),
        3,
        "expected website+address+capacity: {facts:?}"
    );
    for (attribute, _value, provenance, licence, workspace_id, source_ref) in &facts {
        assert_eq!(provenance, "open_directory", "{attribute} provenance");
        assert_eq!(licence.as_deref(), Some("odbl"), "{attribute} licence");
        assert_eq!(*workspace_id, None, "{attribute} must be global");
        assert_eq!(source_ref, "osm:node:555000001");
    }
    assert_eq!(facts[0].0, "address");
    assert_eq!(facts[1].0, "capacity");
    assert_eq!(facts[2].0, "website");

    // ── The email tags produced no rows at all — not even a suppressed one.
    let contactish: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM place_venue_facts \
         WHERE venue_id = $1 \
           AND (attribute LIKE '%email%' OR attribute LIKE '%contact%' \
                OR attribute LIKE '%phone%' OR attribute LIKE 'booking%')",
    )
    .bind(venue)
    .fetch_one(pool)
    .await?;
    assert_eq!(contactish, 0, "an OSM contact tag must never become a fact");

    // ── A re-sweep refreshes the claim rather than stacking a twin. ──────
    let second = sweep(
        pool,
        vec![element(
            555_000_001,
            "Klub Firlej",
            &[("website", "https://firlej.example/new")],
        )],
    );
    let summary = second.sweep_once().await?;
    assert_eq!(summary.written, 1);
    let venue_count: i64 =
        sqlx::query_scalar("SELECT count(*)::bigint FROM place_venues WHERE city_id = $1")
            .bind(city_id)
            .fetch_one(pool)
            .await?;
    assert_eq!(venue_count, 2, "re-sweep minted a duplicate room");
    let website_rows: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM place_venue_facts \
         WHERE venue_id = $1 AND attribute = 'website'",
    )
    .bind(venue)
    .fetch_one(pool)
    .await?;
    assert_eq!(website_rows, 1, "re-sweep duplicated the website fact");
    let website: String = sqlx::query_scalar(
        "SELECT value FROM place_venue_facts \
         WHERE venue_id = $1 AND attribute = 'website'",
    )
    .bind(venue)
    .fetch_one(pool)
    .await?;
    assert_eq!(website, "https://firlej.example/new");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_expired_fact_is_deleted_not_filtered() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run_expiry_case(&database).await
}

async fn run_expiry_case(pool: &PgPool) -> Result<()> {
    let directory = PostgresVenueDirectoryRepository::new(pool.clone());
    let city = sqlx::query_scalar::<_, Uuid>("SELECT id FROM cities WHERE slug = 'wroclaw'")
        .fetch_one(pool)
        .await?;
    let venue = directory
        .upsert_venue(city, "Licensed Room", Some(51.0), Some(17.0))
        .await?;

    // A licensed claim whose clock ran out — and a fresh one beside it, so
    // the sweep has to discriminate rather than truncate.
    PostgresVenueSeedRepository::write_fact(
        pool,
        VenueFactWrite {
            venue_id: venue,
            attribute: "phone",
            value: "+48 000 000 000",
            provenance: "commercial_directory",
            source_ref: "dir:example:1",
            observed_at: None,
            expires_at: Some(time::OffsetDateTime::now_utc() - time::Duration::days(1)),
            workspace_id: None,
            licence: None,
        },
    )
    .await?;
    PostgresVenueSeedRepository::write_fact(
        pool,
        VenueFactWrite {
            venue_id: venue,
            attribute: "website",
            value: "https://licensed-room.example/",
            provenance: "commercial_directory",
            source_ref: "dir:example:1",
            observed_at: None,
            expires_at: Some(time::OffsetDateTime::now_utc() + time::Duration::days(30)),
            workspace_id: None,
            licence: None,
        },
    )
    .await?;
    // A private expired fact dies too — a licence clock is a licence clock.
    PostgresVenueSeedRepository::write_fact(
        pool,
        VenueFactWrite {
            venue_id: venue,
            attribute: "phone",
            value: "+48 000 000 001",
            provenance: "commercial_directory",
            source_ref: "dir:example:1",
            observed_at: None,
            expires_at: Some(time::OffsetDateTime::now_utc() - time::Duration::hours(2)),
            workspace_id: Some(seed_workspace(pool).await?),
            licence: None,
        },
    )
    .await?;

    let deleted = delete_expired_facts(pool).await?;
    assert_eq!(deleted, 2);

    let remaining = facts_for(pool, venue).await?;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].0, "website");

    // The second pass is a no-op — expiry must be idempotent.
    assert_eq!(delete_expired_facts(pool).await?, 0);
    Ok(())
}
