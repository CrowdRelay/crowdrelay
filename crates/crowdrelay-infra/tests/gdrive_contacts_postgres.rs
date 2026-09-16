//! Contact-scan → booking-candidate promote against a real Postgres.
//!
//! What fails here and nowhere else: the city requirement (a candidate with
//! no city can never promote), the venue-registry name match that resolves a
//! room the band already played, and the route-identity dedupe that keeps a
//! re-promote from filing twice.

use std::time::Duration;

use crowdrelay_infra::gdrive::PostgresGDriveRepository;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresGDriveRepository,
    workspace_id: Uuid,
}

async fn fixture(label: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
    let database_url =
        std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").map_err(|error| {
            format!(
                "CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL must target a disposable database: {error}"
            )
        })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

    let workspace_id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id)
        .bind(format!("{label}-{}", workspace_id.simple()))
        .bind("Contacts promote E2E")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO cities (id, slug, name, country_code) \
         VALUES (gen_random_uuid(), 'wroclaw', 'Wroclaw', 'PL') \
         ON CONFLICT (country_code, slug) DO NOTHING",
    )
    .execute(&pool)
    .await?;
    Ok(Fixture {
        repository: PostgresGDriveRepository::new(pool.clone()),
        pool,
        workspace_id,
    })
}

async fn seed_contact(
    fixture: &Fixture,
    email: &str,
    display_name: Option<&str>,
    organization: Option<&str>,
) -> Result<crowdrelay_infra::gdrive::DriveContactRow, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_drive_contacts
            (id, workspace_id, normalized_email, display_name, organization,
             suggested_kind, source_file_id, source_file_name, sources)
        VALUES ($1,$2,$3,$4,$5,'promoter','msg-1','From: Klub X <bookings@klubx.pl> — Re: show','{gmail}')
        "#,
    )
    .bind(id)
    .bind(fixture.workspace_id)
    .bind(email)
    .bind(display_name)
    .bind(organization)
    .execute(&fixture.pool)
    .await?;
    Ok(fixture
        .repository
        .get_contact(fixture.workspace_id, id)
        .await?)
}

#[tokio::test]
#[ignore = "requires a disposable postgres database"]
async fn booking_promote_files_admitted_candidate_with_thread()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("bookpromo").await?;
    let contact = seed_contact(
        &fixture,
        "bookings@klubx.pl",
        Some("Klub X Booking"),
        Some("Klub X"),
    )
    .await?;

    let outcome = fixture
        .repository
        .promote_beacon_booking(fixture.workspace_id, &contact, "promoter", Some("wroclaw"))
        .await?;
    assert_eq!(
        outcome,
        crowdrelay_infra::gdrive::BookingPromoteOutcome::Done
    );

    let row = sqlx::query_as::<_, (String, String, String, String, String, Option<String>)>(
        r#"
        SELECT target_kind, display_name, city_slug, route_value, source, source_reference
        FROM viryaos_booking_candidates
        WHERE workspace_id = $1 AND route_value = 'bookings@klubx.pl'
        "#,
    )
    .bind(fixture.workspace_id)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(row.0, "promoter");
    assert_eq!(row.1, "Klub X Booking");
    assert_eq!(row.2, "wroclaw");
    assert_eq!(row.4, "contact_scan");
    // The thread is the evidence — the reviewer places the contact from it.
    assert!(row.5.unwrap().contains("Klub X"));

    let outcome: String =
        sqlx::query_scalar("SELECT beacon_outcome FROM viryaos_drive_contacts WHERE id = $1")
            .bind(contact.id)
            .fetch_one(&fixture.pool)
            .await?;
    assert_eq!(outcome, "promoted");
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable postgres database"]
async fn booking_promote_without_city_or_match_stays_staged()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("booknocity").await?;
    let contact = seed_contact(&fixture, "agent@bigpromo.pl", Some("Big Promo"), None).await?;

    let outcome = fixture
        .repository
        .promote_beacon_booking(fixture.workspace_id, &contact, "promoter", None)
        .await?;
    assert_eq!(
        outcome,
        crowdrelay_infra::gdrive::BookingPromoteOutcome::CityRequired
    );
    // Nothing filed, nothing marked — the decision is still the operator's.
    let (candidates, outcome): (i64, String) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM viryaos_booking_candidates WHERE workspace_id = $1), \
         (SELECT beacon_outcome FROM viryaos_drive_contacts WHERE id = $2)",
    )
    .bind(fixture.workspace_id)
    .bind(contact.id)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(candidates, 0);
    assert_eq!(outcome, "staged");
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable postgres database"]
async fn booking_promote_resolves_city_from_venue_registry()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("bookvenue").await?;
    // A room the band already played: name matches the contact's org.
    let city_id: Uuid = sqlx::query_scalar("SELECT id FROM cities WHERE slug = 'wroclaw'")
        .fetch_one(&fixture.pool)
        .await?;
    // The trigger files the venue+mark — the registry row a real show makes.
    sqlx::query(
        r#"
        INSERT INTO events (
            id, workspace_id, city_id, slug, title, description, venue,
            timezone, starts_at, doors_at, ends_at, status, published_at
        ) VALUES (
            gen_random_uuid(), $1, $2, 'klub-x-show', 'Show', 'Test',
            'Klub X', 'Europe/Warsaw', now() - interval '30 days',
            now() - interval '31 days', now() - interval '27 days',
            'completed', now() - interval '60 days'
        )
        "#,
    )
    .bind(fixture.workspace_id)
    .bind(city_id)
    .execute(&fixture.pool)
    .await?;

    let contact = seed_contact(&fixture, "booking@klubx.pl", None, Some("klub x")).await?;
    let outcome = fixture
        .repository
        .promote_beacon_booking(fixture.workspace_id, &contact, "venue", None)
        .await?;
    assert_eq!(
        outcome,
        crowdrelay_infra::gdrive::BookingPromoteOutcome::Done
    );
    let (kind, city): (String, String) = sqlx::query_as(
        "SELECT target_kind, city_slug FROM viryaos_booking_candidates \
         WHERE workspace_id = $1 AND route_value = 'booking@klubx.pl'",
    )
    .bind(fixture.workspace_id)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!((kind.as_str(), city.as_str()), ("venue", "wroclaw"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable postgres database"]
async fn booking_promote_dedupes_on_route_identity() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("bookdupe").await?;
    // Discovery already filed a candidate at this route — the contact promote
    // must not file a second row or reset the first one's evidence.
    sqlx::query(
        r#"
        INSERT INTO viryaos_booking_candidates (
            workspace_id, target_kind, display_name, city_slug,
            route_kind, route_value, source, source_reference,
            evidence, fit_basis_points, status
        ) VALUES ($1,'venue','Room (discovered)','wroclaw','email','dup@room.pl',
                  'discovery','https://venue-site.example','published listing',7000,'admitted')
        "#,
    )
    .bind(fixture.workspace_id)
    .execute(&fixture.pool)
    .await?;

    let contact = seed_contact(&fixture, "dup@room.pl", Some("Room"), None).await?;
    let outcome = fixture
        .repository
        .promote_beacon_booking(fixture.workspace_id, &contact, "venue", Some("wroclaw"))
        .await?;
    assert_eq!(
        outcome,
        crowdrelay_infra::gdrive::BookingPromoteOutcome::Done
    );
    let (count, existing_name): (i64, String) = sqlx::query_as(
        "SELECT count(*) OVER (), (SELECT display_name FROM viryaos_booking_candidates \
         WHERE workspace_id = $1 AND route_value = 'dup@room.pl') \
         FROM viryaos_booking_candidates WHERE workspace_id = $1 AND route_value = 'dup@room.pl'",
    )
    .bind(fixture.workspace_id)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(count, 1);
    assert_eq!(existing_name, "Room (discovered)");
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable postgres database"]
async fn booking_promote_rejects_unknown_city_slug() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("bookbadcity").await?;
    let contact = seed_contact(&fixture, "promo@nowhere.pl", Some("Nowhere Promo"), None).await?;
    let outcome = fixture
        .repository
        .promote_beacon_booking(
            fixture.workspace_id,
            &contact,
            "promoter",
            Some("wroclaw-typo"),
        )
        .await?;
    assert_eq!(
        outcome,
        crowdrelay_infra::gdrive::BookingPromoteOutcome::UnknownCity
    );
    let staged: String =
        sqlx::query_scalar("SELECT beacon_outcome FROM viryaos_drive_contacts WHERE id = $1")
            .bind(contact.id)
            .fetch_one(&fixture.pool)
            .await?;
    assert_eq!(staged, "staged");
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable postgres database"]
async fn booking_promote_fills_city_on_existing_candidate() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = fixture("bookfill").await?;
    // Discovery filed the route without a city — the contact promote proves it.
    sqlx::query(
        r#"
        INSERT INTO viryaos_booking_candidates (
            workspace_id, target_kind, display_name, city_slug,
            route_kind, route_value, source, source_reference,
            evidence, fit_basis_points, status
        ) VALUES ($1,'promoter','Big Promo',NULL,'email','fill@promo.pl',
                  'discovery','https://promo.example','published listing',6500,'admitted')
        "#,
    )
    .bind(fixture.workspace_id)
    .execute(&fixture.pool)
    .await?;
    let contact = seed_contact(&fixture, "fill@promo.pl", Some("Big Promo"), None).await?;
    let outcome = fixture
        .repository
        .promote_beacon_booking(fixture.workspace_id, &contact, "promoter", Some("wroclaw"))
        .await?;
    assert_eq!(
        outcome,
        crowdrelay_infra::gdrive::BookingPromoteOutcome::Done
    );
    let city: Option<String> = sqlx::query_scalar(
        "SELECT city_slug FROM viryaos_booking_candidates          WHERE workspace_id = $1 AND route_value = 'fill@promo.pl'",
    )
    .bind(fixture.workspace_id)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(city.as_deref(), Some("wroclaw"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable postgres database"]
async fn booking_promote_does_not_overturn_refused_route() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = fixture("bookrefused").await?;
    sqlx::query(
        r#"
        INSERT INTO viryaos_booking_candidates (
            workspace_id, target_kind, display_name, city_slug,
            route_kind, route_value, source, source_reference,
            evidence, fit_basis_points, status, refusal_reason
        ) VALUES ($1,'venue','Pay To Play Room','wroclaw','email','p2p@room.pl',
                  'discovery','https://p2p.example','published listing',7000,'refused','paid_to_apply')
        "#,
    )
    .bind(fixture.workspace_id)
    .execute(&fixture.pool)
    .await?;
    let contact = seed_contact(&fixture, "p2p@room.pl", Some("P2P Room"), None).await?;
    let outcome = fixture
        .repository
        .promote_beacon_booking(fixture.workspace_id, &contact, "venue", Some("wroclaw"))
        .await?;
    assert_eq!(
        outcome,
        crowdrelay_infra::gdrive::BookingPromoteOutcome::RouteRefused
    );
    // Contact stays staged — the operator sees the promote did not land.
    let staged: String =
        sqlx::query_scalar("SELECT beacon_outcome FROM viryaos_drive_contacts WHERE id = $1")
            .bind(contact.id)
            .fetch_one(&fixture.pool)
            .await?;
    assert_eq!(staged, "staged");
    Ok(())
}
