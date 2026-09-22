//! The 4V.9 booking graph against a real schema (§12-5 entities 4–6).
//!
//! Three objects, one property each that only a database can prove:
//!
//! - **The booking agent** — a `booking_agent`/`talent_buyer`-typed Drive
//!   contact promotes into `booking_agents`, not the city-scoped
//!   candidate queue, and the same address promoted twice is still one agent.
//! - **The festival edition** — the window CHECK refuses an inside-out
//!   window and the composite FK refuses an edition hung on another
//!   workspace's target: cross-tenant attach fails by construction, not by
//!   application care.
//! - **The promoter↔venue edge** — one promoter links two rooms and the
//!   snapshot's `linked_venue_ids` is the union of the primary `venue_id`
//!   and every edge, never a cross-tenant leak.

use crate::common;

use std::time::Duration;

use crowdrelay_application::IdempotencyKey;
use crowdrelay_application::autopilot::{
    AutopilotBookingStateRepository, AutopilotDecisionRepository, UpsertFestivalEdition,
};
use crowdrelay_domain::{BookingTargetId, VenueId, WorkspaceId};
use crowdrelay_infra::{
    autopilot::PostgresAutopilotRepository, config::DatabaseConfig,
    gdrive::PostgresGDriveRepository,
};
use sqlx::PgPool;
use uuid::Uuid;

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

/// Cities are unique on `(country_code, slug)` and the catalogue is seeded —
/// insert is the fallback, select is the answer.
async fn seed_city(pool: &PgPool, slug: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO cities (slug, name, country_code) VALUES ($1, $2, 'PL')
         ON CONFLICT (country_code, slug) DO NOTHING",
    )
    .bind(slug)
    .bind(slug)
    .execute(pool)
    .await?;
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM cities WHERE country_code = 'PL' AND slug = $1",
    )
    .bind(slug)
    .fetch_one(pool)
    .await?)
}

async fn seed_venue(
    pool: &PgPool,
    city_id: Uuid,
    display_name: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO place_venues (city_id, name_key, display_name)
         VALUES ($1, place_venue_key($2), $2) RETURNING id",
    )
    .bind(city_id)
    .bind(display_name)
    .fetch_one(pool)
    .await?)
}

async fn seed_target(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    kind: &str,
    display_name: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO booking_targets
             (workspace_id, city_id, target_kind, display_name, contact_email)
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(kind)
    .bind(display_name)
    .bind(format!("booking+{}@example.com", Uuid::now_v7().simple()))
    .fetch_one(pool)
    .await?)
}

/// A staged contact the way the Drive extractor files one — the kind the
/// sheet's own type column produced.
async fn seed_contact(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
    kind: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        r#"INSERT INTO drive_contacts
            (workspace_id, normalized_email, display_name, organization,
             suggested_kind, source_file_id, source_file_name, sources)
        VALUES ($1,$2,'Wera Hroza','Roadtone Agency',$3,'file-1','agents.xlsx','{gdrive}')
        RETURNING id"#,
    )
    .bind(workspace_id)
    .bind(email)
    .bind(kind)
    .fetch_one(pool)
    .await?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_agent_kind_contact_promotes_into_booking_agents()
-> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run_agent_case(&database).await
}

fn repository(pool: &PgPool, url: &str) -> PostgresAutopilotRepository {
    PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: url.to_owned(),
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        },
    )
}

async fn run_agent_case(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = seed_workspace(pool).await?;
    let contact_id =
        seed_contact(pool, workspace, "wera@roadtone.example", "booking_agent").await?;
    let repo = PostgresGDriveRepository::new(pool.clone());
    let contact = repo.get_contact(workspace, contact_id).await?;

    repo.promote_beacon_agent(workspace, &contact).await?;

    // The agent row: name from the sheet, agency from its organization
    // column, the address as the dedup key.
    let (name, agency, active): (String, Option<String>, bool) = sqlx::query_as(
        "SELECT name, agency, active FROM booking_agents
         WHERE workspace_id = $1 AND contact_email = 'wera@roadtone.example'",
    )
    .bind(workspace)
    .fetch_one(pool)
    .await?;
    assert_eq!(name, "Wera Hroza");
    assert_eq!(agency.as_deref(), Some("Roadtone Agency"));
    assert!(active, "a fresh agent is active");

    // The contact is filed as promoted — a re-scan never re-suggests it.
    let outcome: String = sqlx::query_scalar(
        "SELECT beacon_outcome FROM drive_contacts WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace)
    .bind(contact_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(outcome, "promoted");

    // The agent went nowhere near the city-scoped candidate queue.
    let candidates: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM booking_candidates WHERE workspace_id = $1",
    )
    .bind(workspace)
    .fetch_one(pool)
    .await?;
    assert_eq!(candidates, 0, "an agent must never file as a candidate");

    // A second promote of the same address refreshes the row — one agent,
    // not a twin.
    let contact = repo.get_contact(workspace, contact_id).await?;
    repo.promote_beacon_agent(workspace, &contact).await?;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*)::bigint FROM booking_agents WHERE workspace_id = $1")
            .bind(workspace)
            .fetch_one(pool)
            .await?;
    assert_eq!(count, 1, "one address is one agent");

    // A talent_buyer lands the same way — the intake files both spellings
    // to the same entity.
    let buyer_id = seed_contact(pool, workspace, "milan@roadtone.example", "talent_buyer").await?;
    let contact = repo.get_contact(workspace, buyer_id).await?;
    repo.promote_beacon_agent(workspace, &contact).await?;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*)::bigint FROM booking_agents WHERE workspace_id = $1")
            .bind(workspace)
            .fetch_one(pool)
            .await?;
    assert_eq!(count, 2, "the talent buyer is a second agent row");

    // Another workspace's promote cannot see this workspace's contact.
    let other = seed_workspace(pool).await?;
    assert!(
        repo.get_contact(other, contact_id).await.is_err(),
        "a contact read across workspaces answered"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_edition_window_is_checked_and_never_crosses_tenants()
-> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run_edition_case(&database).await
}

async fn run_edition_case(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = seed_workspace(pool).await?;
    let other = seed_workspace(pool).await?;
    let wroclaw = seed_city(pool, "wroclaw").await?;
    let festival = seed_target(pool, workspace, wroclaw, "festival", "Brutal Assault").await?;

    // A window that opens after it closes is not a window.
    let inside_out = sqlx::query(
        "INSERT INTO festival_editions
             (workspace_id, target_id, edition_label, application_opens_at, application_closes_at)
         VALUES ($1, $2, 'BA 2027', now() + interval '60 days', now() + interval '30 days')",
    )
    .bind(workspace)
    .bind(festival)
    .execute(pool)
    .await;
    assert!(
        inside_out.is_err(),
        "the window CHECK admitted an inside-out window"
    );

    // An edition hung on another workspace's target is a cross-tenant
    // attach — the composite (workspace_id, target_id) FK refuses it
    // outright rather than trusting every writer to filter.
    let cross_tenant = sqlx::query(
        "INSERT INTO festival_editions (workspace_id, target_id, edition_label)
         VALUES ($1, $2, 'Stolen Edition')",
    )
    .bind(other)
    .bind(festival)
    .execute(pool)
    .await;
    assert!(
        cross_tenant.is_err(),
        "the composite FK admitted a cross-tenant edition"
    );

    // The honest edition writes, and a half-open window is legal — a
    // festival that only published its close is still schedulable.
    sqlx::query(
        "INSERT INTO festival_editions
             (workspace_id, target_id, edition_label, starts_at, application_closes_at, lineup_url)
         VALUES ($1, $2, 'BA 2027', now() + interval '90 days', now() + interval '11 days',
                 'https://brutalassault.cz/lineup')",
    )
    .bind(workspace)
    .bind(festival)
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_promoter_edge_unions_rooms_and_never_leaks_tenants()
-> Result<(), Box<dyn std::error::Error>> {
    let (database, url) = common::test_pool_with_url("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let repository = repository(&database, &url);

    run_edge_case(&database, &repository).await
}

async fn run_edge_case(
    pool: &PgPool,
    repository: &PostgresAutopilotRepository,
) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = seed_workspace(pool).await?;
    let other = seed_workspace(pool).await?;
    let workspace_id = WorkspaceId::from_uuid(workspace);
    let wroclaw = seed_city(pool, "wroclaw").await?;
    let praha = seed_city(pool, "praha").await?;
    let klub_x = seed_venue(pool, wroclaw, "Klub X").await?;
    let klub_y = seed_venue(pool, wroclaw, "Klub Y").await?;
    let lucerna = seed_venue(pool, praha, "Lucerna").await?;
    let target = |id: Uuid| BookingTargetId::from_uuid(id);
    let venue = |id: Uuid| VenueId::from_uuid(id);

    // The promoter's display name is the booker, not a room — its primary
    // venue_id stays NULL and the edges are the whole of its reach.
    let promoter = seed_target(pool, workspace, wroclaw, "promoter", "Klub X Booker").await?;
    repository
        .link_target_venue(workspace_id, target(promoter), venue(klub_y))
        .await?;
    repository
        .link_target_venue(workspace_id, target(promoter), venue(lucerna))
        .await?;
    // Re-linking is the idempotent answer — `Ok(false)`, still one row.
    assert!(
        !repository
            .link_target_venue(workspace_id, target(promoter), venue(lucerna))
            .await?,
        "a replayed link reported a fresh edge"
    );

    let snapshots = repository
        .load_booking_target_snapshots(workspace_id, time::OffsetDateTime::now_utc())
        .await?;
    let snapshot = snapshots
        .iter()
        .find(|s| s.target_id.into_uuid() == promoter)
        .expect("the seeded target is in the snapshot");
    let linked: Vec<Uuid> = snapshot
        .linked_venue_ids
        .iter()
        .map(|v| v.into_uuid())
        .collect();
    assert_eq!(linked.len(), 2, "expected the two edge rooms: {linked:?}");
    assert!(linked.contains(&klub_y) && linked.contains(&lucerna));
    assert!(
        !linked.contains(&klub_x),
        "Klub X was never linked — the display name is the booker, not the room"
    );

    // A venue-typed target whose name matches a room carries its primary
    // link — the union includes primary ∪ edges together.
    let venue_target = seed_target(pool, workspace, wroclaw, "venue", "Klub X").await?;
    assert_eq!(
        sqlx::query_scalar::<_, Option<Uuid>>("SELECT venue_id FROM booking_targets WHERE id = $1")
            .bind(venue_target)
            .fetch_one(pool)
            .await?,
        Some(klub_x),
        "the name trigger did not resolve the primary room"
    );
    repository
        .link_target_venue(workspace_id, target(venue_target), venue(klub_y))
        .await?;
    let snapshots = repository
        .load_booking_target_snapshots(workspace_id, time::OffsetDateTime::now_utc())
        .await?;
    let snapshot = snapshots
        .iter()
        .find(|s| s.target_id.into_uuid() == venue_target)
        .expect("the venue target is in the snapshot");
    let linked: Vec<Uuid> = snapshot
        .linked_venue_ids
        .iter()
        .map(|v| v.into_uuid())
        .collect();
    assert!(
        linked.contains(&klub_x) && linked.contains(&klub_y) && linked.len() == 2,
        "the union is primary ∪ edges: {linked:?}"
    );

    // Unlink is tenant-scoped and honest: absent edge → false; the same
    // target under another workspace is NotFound — the pre-read, not luck,
    // keeps workspace B from unlinking workspace A's room.
    assert!(
        repository
            .unlink_target_venue(workspace_id, target(venue_target), venue(klub_y))
            .await?,
        "the unlink of a live edge did not report it"
    );
    assert!(
        !repository
            .unlink_target_venue(workspace_id, target(venue_target), venue(klub_y))
            .await?,
        "the second unlink should read as already-absent"
    );
    assert!(
        repository
            .link_target_venue(
                WorkspaceId::from_uuid(other),
                target(promoter),
                venue(klub_y),
            )
            .await
            .is_err(),
        "workspace B attached a room to workspace A's target"
    );
    let foreign_edges: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM booking_target_venues WHERE workspace_id = $1",
    )
    .bind(other)
    .fetch_one(pool)
    .await?;
    assert_eq!(foreign_edges, 0, "a cross-tenant edge exists");

    // And the festival answer on the same load path: the edition closing in
    // eleven days reads as the snapshot's days-until-close.
    let festival = seed_target(pool, workspace, wroclaw, "festival", "Brutal Assault").await?;
    sqlx::query(
        "INSERT INTO festival_editions
             (workspace_id, target_id, edition_label, application_closes_at)
         VALUES ($1, $2, 'BA 2027', now() + interval '11 days')",
    )
    .bind(workspace)
    .bind(festival)
    .execute(pool)
    .await?;
    let snapshots = repository
        .load_booking_target_snapshots(workspace_id, time::OffsetDateTime::now_utc())
        .await?;
    let festival_row = snapshots
        .iter()
        .find(|s| s.target_id.into_uuid() == festival)
        .expect("the festival target is in the snapshot");
    assert_eq!(
        festival_row.days_until_application_close,
        Some(11),
        "the window's close did not reach the snapshot"
    );
    assert!(
        festival_row.next_application_closes_at.is_some(),
        "the close timestamp — the edition's identity for a decision key — did not reach the snapshot"
    );
    let promoter_row = snapshots
        .iter()
        .find(|s| s.target_id.into_uuid() == promoter)
        .expect("the promoter is in the snapshot");
    assert_eq!(
        promoter_row.days_until_application_close, None,
        "a non-festival carries no window"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_edition_writes_replays_and_stays_on_festivals() -> Result<(), Box<dyn std::error::Error>>
{
    let (database, url) = common::test_pool_with_url("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let repository = repository(&database, &url);

    run_edition_upsert_case(&database, &repository).await
}

async fn run_edition_upsert_case(
    pool: &PgPool,
    repository: &PostgresAutopilotRepository,
) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = seed_workspace(pool).await?;
    let workspace_id = WorkspaceId::from_uuid(workspace);
    let wroclaw = seed_city(pool, "wroclaw").await?;
    let festival = seed_target(pool, workspace, wroclaw, "festival", "Brutal Assault").await?;
    let venue = seed_target(pool, workspace, wroclaw, "venue", "Klub X").await?;
    let key = IdempotencyKey::parse(format!("edition-{}", Uuid::now_v7().simple()))?;
    let closes = time::OffsetDateTime::now_utc() + time::Duration::days(30);

    let command = UpsertFestivalEdition {
        target_id: BookingTargetId::from_uuid(festival),
        edition_label: "BA 2027".to_owned(),
        starts_at: Some(closes + time::Duration::days(60)),
        application_opens_at: None,
        application_closes_at: Some(closes),
        lineup_url: Some("https://brutalassault.cz/lineup".to_owned()),
    };
    let first = repository
        .upsert_festival_edition(workspace_id, command, &key, None)
        .await?;
    assert!(!first.replayed);

    // The same key replays the same answer — the operator's retry is a
    // no-op, not a second edition.
    let replay = repository
        .upsert_festival_edition(
            workspace_id,
            UpsertFestivalEdition {
                target_id: BookingTargetId::from_uuid(festival),
                edition_label: "BA 2027".to_owned(),
                starts_at: Some(closes + time::Duration::days(60)),
                application_opens_at: None,
                application_closes_at: Some(closes),
                lineup_url: Some("https://brutalassault.cz/lineup".to_owned()),
            },
            &key,
            None,
        )
        .await?;
    assert!(replay.replayed, "the same key must report a replay");
    assert_eq!(replay.edition_id, first.edition_id);

    // A corrected window under a new key rewrites the same edition row —
    // natural-key upsert, not a sibling.
    let corrected = repository
        .upsert_festival_edition(
            workspace_id,
            UpsertFestivalEdition {
                target_id: BookingTargetId::from_uuid(festival),
                edition_label: "BA 2027".to_owned(),
                starts_at: None,
                application_opens_at: None,
                application_closes_at: Some(closes + time::Duration::days(7)),
                lineup_url: None,
            },
            &IdempotencyKey::parse(format!("edition-fix-{}", Uuid::now_v7().simple()))?,
            None,
        )
        .await?;
    assert_eq!(corrected.edition_id, first.edition_id);
    let rows = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM festival_editions WHERE workspace_id = $1",
    )
    .bind(workspace)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        rows, 1,
        "the correction must rewrite the edition, not add one"
    );
    let stored_closes = sqlx::query_scalar::<_, time::OffsetDateTime>(
        "SELECT application_closes_at FROM festival_editions          WHERE workspace_id = $1 AND target_id = $2",
    )
    .bind(workspace)
    .bind(festival)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        stored_closes.unix_timestamp(),
        (closes + time::Duration::days(7)).unix_timestamp(),
        "the corrected close did not land"
    );

    // An edition hung on a venue is nonsense the writer refuses, before the
    // FK would even run — the honest answer is a conflict, not a constraint
    // error.
    let wrong_kind = repository
        .upsert_festival_edition(
            workspace_id,
            UpsertFestivalEdition {
                target_id: BookingTargetId::from_uuid(venue),
                edition_label: "Not An Edition".to_owned(),
                starts_at: None,
                application_opens_at: None,
                application_closes_at: Some(closes),
                lineup_url: None,
            },
            &IdempotencyKey::parse(format!("edition-venue-{}", Uuid::now_v7().simple()))?,
            None,
        )
        .await;
    assert!(wrong_kind.is_err(), "an edition on a venue must refuse");

    // And a foreign workspace's festival answers the same 404 every other
    // scoped write does.
    let other = seed_workspace(pool).await?;
    let foreign = repository
        .upsert_festival_edition(
            WorkspaceId::from_uuid(other),
            UpsertFestivalEdition {
                target_id: BookingTargetId::from_uuid(festival),
                edition_label: "Stolen".to_owned(),
                starts_at: None,
                application_opens_at: None,
                application_closes_at: Some(closes),
                lineup_url: None,
            },
            &IdempotencyKey::parse(format!("edition-x-{}", Uuid::now_v7().simple()))?,
            None,
        )
        .await;
    assert!(foreign.is_err(), "a cross-tenant edition write must refuse");
    Ok(())
}
