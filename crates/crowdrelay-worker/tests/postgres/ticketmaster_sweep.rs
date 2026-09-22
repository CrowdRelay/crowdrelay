//! The Ticketmaster sweep against a real schema (§12-3, Sprint 4V.9).
//!
//! The wire is stubbed — what matters is what a Discovery-shaped event
//! leaves on disk and what it must never leave:
//!
//! - a `('ticketmaster', tm_venue_id)` anchor on the resolved room, an
//!   `event_evidence` fact carrying the show, peer acts mint-or-linked on the
//!   normalized name with an MBID when the attraction carries one, and peer
//!   genres attributed `event_evidence` / `ticketmaster:{event_id}`;
//! - a room the registry does not hold mints NOTHING — no venue, no
//!   identifier, no fact, no peer act (a listings feed is evidence, never
//!   identity);
//! - nothing tenant-owned: no `event_acts`, no contacts, no capacity claims;
//! - a re-sweep refreshes rather than duplicates.

use crate::common;

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use async_trait::async_trait;
use crowdrelay_infra::venue_directory::PostgresVenueDirectoryRepository;
use crowdrelay_worker::{
    osm_venue_sweep::SweepCity,
    ticketmaster_sweep::{
        TicketmasterProvider, TicketmasterSweepWorker, TmEvent, parse_events_body,
    },
};
use sqlx::PgPool;
use uuid::Uuid;

/// Answers every city with the same canned Discovery page — the payload is
/// parsed through `parse_events_body`, so the serde shape is exercised too,
/// not just the database writes.
struct CannedProvider {
    body: &'static str,
}

#[async_trait]
impl TicketmasterProvider for CannedProvider {
    async fn events_in(&self, _city: &SweepCity) -> Result<Vec<TmEvent>> {
        parse_events_body(self.body.as_bytes())
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

/// A city with a pin and one completed show — the sweep's selection rule
/// made concrete, identical to the OSM sweep's set by construction.
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

/// A room the registry already holds — the sweep may only write onto it.
async fn seed_venue(pool: &PgPool, city_id: Uuid, display_name: &str) -> Result<Uuid> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO place_venues (city_id, name_key, display_name)
         VALUES ($1, place_venue_key($2), $2) RETURNING id",
    )
    .bind(city_id)
    .bind(display_name)
    .fetch_one(pool)
    .await?)
}

fn sweep(pool: &PgPool, body: &'static str) -> TicketmasterSweepWorker {
    TicketmasterSweepWorker::new(
        pool.clone(),
        Arc::new(CannedProvider { body }),
        Duration::from_secs(60),
        Duration::from_secs(10),
    )
}

/// A Discovery page the way the API actually shapes it — one event at a
/// known room, two attractions (one with an MBID link), genre and subGenre.
const EVENT_AT_KNOWN_ROOM: &str = r#"{
    "_embedded": {
        "events": [{
            "id": "1AfZA5QGkdtZ8ME",
            "name": "Gatecreeper live",
            "dates": {"start": {"dateTime": "2026-11-14T19:00:00Z", "localDate": "2026-11-14"}},
            "classifications": [{
                "segment": {"name": "Music"},
                "genre": {"name": "Metal"},
                "subGenre": {"name": "Death Metal"}
            }],
            "_embedded": {
                "venues": [{"id": "Kovv9171A97", "name": "Klub Firlej"}],
                "attractions": [
                    {
                        "name": "Gatecreeper",
                        "externalLinks": {
                            "musicbrainz": [{"url": "https://musicbrainz.org/artist/5be1f2ef-cacd-46cd-97d9-95f4b35d06ff"}]
                        }
                    },
                    {"name": "Frozen Soul"}
                ]
            }
        }]
    }
}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_tm_event_writes_evidence_onto_a_room_that_exists() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run_sweep_cases(&database).await
}

async fn run_sweep_cases(pool: &PgPool) -> Result<()> {
    let workspace = seed_workspace(pool).await?;
    let city_id = city_with_a_show(pool, workspace, "tm-sweep-city").await?;
    let venue = seed_venue(pool, city_id, "Klub Firlej").await?;
    let venue_count_before: i64 = sqlx::query_scalar("SELECT count(*)::bigint FROM place_venues")
        .fetch_one(pool)
        .await?;

    let summary = sweep(pool, EVENT_AT_KNOWN_ROOM).sweep_once().await?;
    assert_eq!(summary.cities, 1);
    assert_eq!(summary.events, 1);
    assert_eq!(summary.written, 1);
    assert_eq!(summary.skipped, 0);
    assert_eq!(summary.failed, 0);

    // ── The anchor: ('ticketmaster', tm id) → the existing room. ─────────
    let directory = PostgresVenueDirectoryRepository::new(pool.clone());
    assert_eq!(
        directory
            .resolve_identifier("ticketmaster", "Kovv9171A97")
            .await?,
        Some(venue),
        "the ticketmaster anchor did not land on the existing room"
    );

    // ── The event_evidence fact: global, named, dated, sourced. ──────────
    let (attribute, provenance, source_ref, fact_workspace): (
        String,
        String,
        String,
        Option<Uuid>,
    ) = sqlx::query_as(
        "SELECT attribute, provenance, source_ref, workspace_id
             FROM place_venue_facts WHERE venue_id = $1",
    )
    .bind(venue)
    .fetch_one(pool)
    .await?;
    assert_eq!(attribute, "show");
    assert_eq!(provenance, "event_evidence");
    assert_eq!(source_ref, "ticketmaster:1AfZA5QGkdtZ8ME");
    assert_eq!(
        fact_workspace, None,
        "the evidence is global, not a tenant's"
    );
    let value: String = sqlx::query_scalar(
        "SELECT value FROM place_venue_facts WHERE venue_id = $1 AND attribute = 'show'",
    )
    .bind(venue)
    .fetch_one(pool)
    .await?;
    assert!(
        value.contains("Gatecreeper") && value.contains("2026-11-14"),
        "{value}"
    );

    // ── No venue was minted: the count is what it was. ───────────────────
    let venue_count_after: i64 = sqlx::query_scalar("SELECT count(*)::bigint FROM place_venues")
        .fetch_one(pool)
        .await?;
    assert_eq!(
        venue_count_after, venue_count_before,
        "the sweep minted a room"
    );

    // ── The bill's acts are peers: one minted with its MBID, one without. ─
    let gatecreeper: (Uuid, String, Option<Uuid>) = sqlx::query_as(
        "SELECT id, display_name, mbid FROM place_peer_acts WHERE name_key = place_venue_key('Gatecreeper')",
    )
    .fetch_one(pool)
    .await
    .context("Gatecreeper was not minted as a peer act")?;
    assert_eq!(gatecreeper.1, "Gatecreeper");
    assert_eq!(
        gatecreeper.2,
        Some(Uuid::parse_str("5be1f2ef-cacd-46cd-97d9-95f4b35d06ff")?),
        "the MBID anchor did not land"
    );
    let frozen: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM place_peer_acts WHERE name_key = place_venue_key('Frozen Soul')",
    )
    .fetch_optional(pool)
    .await?;
    let frozen = frozen.context("Frozen Soul was not minted")?;

    // ── Genre tags attributed to the event's classifications. ────────────
    let tags: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT genre_tag, provenance, source_ref FROM place_peer_act_genres
         WHERE peer_act_id = $1 ORDER BY genre_tag",
    )
    .bind(gatecreeper.0)
    .fetch_all(pool)
    .await?;
    assert_eq!(
        tags,
        vec![
            (
                "Death Metal".to_owned(),
                "event_evidence".to_owned(),
                "ticketmaster:1AfZA5QGkdtZ8ME".to_owned()
            ),
            (
                "Metal".to_owned(),
                "event_evidence".to_owned(),
                "ticketmaster:1AfZA5QGkdtZ8ME".to_owned()
            ),
        ],
        "genre tags are the event's own classifications"
    );
    let frozen_tags: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM place_peer_act_genres WHERE peer_act_id = $1",
    )
    .bind(frozen)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        frozen_tags, 2,
        "the second act carries the same bill's tags"
    );

    // ── Nothing tenant-owned was touched. ────────────────────────────────
    let tenant_rows: i64 = sqlx::query_scalar("SELECT count(*)::bigint FROM event_acts")
        .fetch_one(pool)
        .await?;
    assert_eq!(
        tenant_rows, 0,
        "a Ticketmaster show is never a tenant's bill"
    );

    // ── A re-sweep refreshes rather than duplicates. ─────────────────────
    let summary = sweep(pool, EVENT_AT_KNOWN_ROOM).sweep_once().await?;
    assert_eq!(summary.written, 1);
    let fact_count: i64 =
        sqlx::query_scalar("SELECT count(*)::bigint FROM place_venue_facts WHERE venue_id = $1")
            .bind(venue)
            .fetch_one(pool)
            .await?;
    assert_eq!(fact_count, 1, "the re-sweep stacked a twin fact");
    let act_count: i64 = sqlx::query_scalar("SELECT count(*)::bigint FROM place_peer_acts")
        .fetch_one(pool)
        .await?;
    assert_eq!(act_count, 2, "the re-sweep minted twin acts");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_unknown_room_mints_nothing() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run_unknown_case(&database).await
}

async fn run_unknown_case(pool: &PgPool) -> Result<()> {
    let workspace = seed_workspace(pool).await?;
    let city_id = city_with_a_show(pool, workspace, "tm-unknown-city").await?;
    let venue_count_before: i64 =
        sqlx::query_scalar("SELECT count(*)::bigint FROM place_venues WHERE city_id = $1")
            .bind(city_id)
            .fetch_one(pool)
            .await?;

    // A show at a room nobody holds: no name match, no anchor — nothing
    // is written, not even the peer acts, because "mints NOTHING" is total.
    let body = r#"{
        "_embedded": {
            "events": [{
                "id": "ZZunknown9",
                "name": "Bands at a Mystery Room",
                "dates": {"start": {"localDate": "2027-01-01"}},
                "classifications": [{"genre": {"name": "Metal"}}],
                "_embedded": {
                    "venues": [{"id": "Kvvvunknown", "name": "A Room Nobody Has"}],
                    "attractions": [{"name": "Spectral Wound"}]
                }
            }]
        }
    }"#;
    let worker = TicketmasterSweepWorker::new(
        pool.clone(),
        Arc::new(CannedProvider { body }),
        Duration::from_secs(60),
        Duration::from_secs(10),
    );
    let summary = worker.sweep_once().await?;
    assert_eq!(summary.skipped, 1);
    assert_eq!(summary.written, 0);

    let venue_count_after: i64 =
        sqlx::query_scalar("SELECT count(*)::bigint FROM place_venues WHERE city_id = $1")
            .bind(city_id)
            .fetch_one(pool)
            .await?;
    assert_eq!(
        venue_count_after, venue_count_before,
        "an unknown room was minted"
    );
    let anchors: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM place_venue_identifiers WHERE scheme = 'ticketmaster'",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(
        anchors, 0,
        "an anchor was written for a room we do not hold"
    );
    let facts: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM place_venue_facts WHERE provenance = 'event_evidence'",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(facts, 0, "a fact was written for a room we do not hold");
    let acts: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM place_peer_acts WHERE name_key = place_venue_key('Spectral Wound')",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(acts, 0, "the unknown show still minted an act");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_anchor_answers_before_the_name() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run_anchor_case(&database).await
}

/// A Ticketmaster venue id already anchored to a room wins over the name —
/// even when the event names a different-looking room, the anchor is the
/// resolution rule (§12-4).
async fn run_anchor_case(pool: &PgPool) -> Result<()> {
    let workspace = seed_workspace(pool).await?;
    let city_id = city_with_a_show(pool, workspace, "tm-anchor-city").await?;
    let venue = seed_venue(pool, city_id, "Klub Firlej").await?;
    PostgresVenueDirectoryRepository::new(pool.clone())
        .link_venue_identifier(venue, "ticketmaster", "Kovv9171A97")
        .await?;

    // The event names the room by a spelling that matches nothing — the
    // anchor still resolves, and the fact lands on the anchored room.
    let body = r#"{
        "_embedded": {
            "events": [{
                "id": "anchored1",
                "name": "Anchored Show",
                "_embedded": {
                    "venues": [{"id": "Kovv9171A97", "name": "Firlej Klub Muzyczny"}]
                }
            }]
        }
    }"#;
    let worker = TicketmasterSweepWorker::new(
        pool.clone(),
        Arc::new(CannedProvider { body }),
        Duration::from_secs(60),
        Duration::from_secs(10),
    );
    let summary = worker.sweep_once().await?;
    assert_eq!(summary.written, 1);
    let facts: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM place_venue_facts
         WHERE venue_id = $1 AND provenance = 'event_evidence'",
    )
    .bind(venue)
    .fetch_one(pool)
    .await?;
    assert_eq!(facts, 1, "the anchored room did not receive the fact");
    Ok(())
}
