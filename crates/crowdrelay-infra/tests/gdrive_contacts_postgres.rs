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
        INSERT INTO drive_contacts
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

async fn seed_contact_city(
    fixture: &Fixture,
    email: &str,
    display_name: Option<&str>,
    city: Option<&str>,
) -> Result<crowdrelay_infra::gdrive::DriveContactRow, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO drive_contacts
            (id, workspace_id, normalized_email, display_name,
             suggested_kind, city, source_file_id, source_file_name, sources)
        VALUES ($1,$2,$3,$4,'venue',$5,'file-1','rooms.xlsx','{gdrive}')
        "#,
    )
    .bind(id)
    .bind(fixture.workspace_id)
    .bind(email)
    .bind(display_name)
    .bind(city)
    .execute(&fixture.pool)
    .await?;
    Ok(fixture
        .repository
        .get_contact(fixture.workspace_id, id)
        .await?)
}

#[tokio::test]
#[ignore = "requires a disposable postgres database"]
async fn staged_city_resolves_on_booking_promote() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("stagedcity").await?;
    // The sheet said where — no venue match, no typed slug needed.
    let contact =
        seed_contact_city(&fixture, "room@kluby.pl", Some("Klub Y"), Some("wroclaw")).await?;
    let outcome = fixture
        .repository
        .promote_beacon_booking(fixture.workspace_id, &contact, "venue", None)
        .await?;
    assert_eq!(
        outcome,
        crowdrelay_infra::gdrive::BookingPromoteOutcome::Done
    );
    let city: String = sqlx::query_scalar(
        "SELECT city_slug FROM booking_candidates \
         WHERE workspace_id = $1 AND route_value = 'room@kluby.pl'",
    )
    .bind(fixture.workspace_id)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(city, "wroclaw");
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable postgres database"]
async fn staged_city_name_resolves_on_booking_promote() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("stagedname").await?;
    // Sheets say "Wroclaw", not "wroclaw" — the name resolves the same slug.
    let contact =
        seed_contact_city(&fixture, "room@klubz.pl", Some("Klub Z"), Some("Wroclaw")).await?;
    let outcome = fixture
        .repository
        .promote_beacon_booking(fixture.workspace_id, &contact, "venue", None)
        .await?;
    assert_eq!(
        outcome,
        crowdrelay_infra::gdrive::BookingPromoteOutcome::Done
    );
    let city: String = sqlx::query_scalar(
        "SELECT city_slug FROM booking_candidates \
         WHERE workspace_id = $1 AND route_value = 'room@klubz.pl'",
    )
    .bind(fixture.workspace_id)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(city, "wroclaw");
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable postgres database"]
async fn staged_city_that_names_nothing_still_needs_a_city()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("stagednone").await?;
    // A sheet value no catalogue city matches is a hint, not a city — the
    // promote stays staged rather than guessing a place.
    let contact = seed_contact_city(
        &fixture,
        "room@nowhere.pl",
        Some("Nowhere"),
        Some("Nowheresville"),
    )
    .await?;
    let outcome = fixture
        .repository
        .promote_beacon_booking(fixture.workspace_id, &contact, "venue", None)
        .await?;
    assert_eq!(
        outcome,
        crowdrelay_infra::gdrive::BookingPromoteOutcome::CityRequired
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable postgres database"]
async fn explicit_city_beats_the_staged_one() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("explicitcity").await?;
    sqlx::query(
        "INSERT INTO cities (id, slug, name, country_code) \
         VALUES (gen_random_uuid(), 'poznan', 'Poznan', 'PL') \
         ON CONFLICT (country_code, slug) DO NOTHING",
    )
    .execute(&fixture.pool)
    .await?;
    // The sheet says Wroclaw; the operator says Poznan. The explicit slug
    // is the operator's call and wins outright.
    let contact =
        seed_contact_city(&fixture, "room@klubw.pl", Some("Klub W"), Some("Wroclaw")).await?;
    let outcome = fixture
        .repository
        .promote_beacon_booking(fixture.workspace_id, &contact, "venue", Some("poznan"))
        .await?;
    assert_eq!(
        outcome,
        crowdrelay_infra::gdrive::BookingPromoteOutcome::Done
    );
    let city: String = sqlx::query_scalar(
        "SELECT city_slug FROM booking_candidates \
         WHERE workspace_id = $1 AND route_value = 'room@klubw.pl'",
    )
    .bind(fixture.workspace_id)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(city, "poznan");
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable postgres database"]
async fn unmatched_staged_city_still_falls_back_to_the_venue_registry()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("stagedvenue").await?;
    // The sheet's city names nothing in the catalogue, but the room is one
    // the band already played — the registry's unique match is the better
    // evidence and still resolves the city.
    let city_id: Uuid = sqlx::query_scalar("SELECT id FROM cities WHERE slug = 'wroclaw'")
        .fetch_one(&fixture.pool)
        .await?;
    sqlx::query(
        r#"
        INSERT INTO events (
            id, workspace_id, city_id, slug, title, description, venue,
            timezone, starts_at, doors_at, ends_at, status, published_at
        ) VALUES (
            gen_random_uuid(), $1, $2, 'klub-v-show', 'Show', 'Test',
            'Klub V', 'Europe/Warsaw', now() - interval '30 days',
            now() - interval '31 days', now() - interval '27 days',
            'completed', now() - interval '60 days'
        )
        "#,
    )
    .bind(fixture.workspace_id)
    .bind(city_id)
    .execute(&fixture.pool)
    .await?;
    let contact = seed_contact_city(
        &fixture,
        "booking@klubv.pl",
        Some("Klub V"),
        Some("Nowheresville"),
    )
    .await?;
    let outcome = fixture
        .repository
        .promote_beacon_booking(fixture.workspace_id, &contact, "venue", None)
        .await?;
    assert_eq!(
        outcome,
        crowdrelay_infra::gdrive::BookingPromoteOutcome::Done
    );
    let city: String = sqlx::query_scalar(
        "SELECT city_slug FROM booking_candidates \
         WHERE workspace_id = $1 AND route_value = 'booking@klubv.pl'",
    )
    .bind(fixture.workspace_id)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(city, "wroclaw");
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable postgres database"]
async fn upsert_keeps_the_sheet_city() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("upsertcity").await?;
    fixture
        .repository
        .upsert_contacts_for_source(
            fixture.workspace_id,
            "gdrive",
            "file-1",
            "rooms.xlsx",
            &[crowdrelay_domain::drive_contacts::ExtractedContact {
                email: "venue@klubq.pl".to_owned(),
                display_name: Some("Klub Q".to_owned()),
                organization: None,
                phone: None,
                suggested_kind: Some("venue".to_owned()),
                city: Some("Wroclaw".to_owned()),
                notes: None,
            }],
            true,
        )
        .await?;
    // A second file sighting the same address without the column keeps it —
    // the new file is not the truth about the old one's city.
    fixture
        .repository
        .upsert_contacts_for_source(
            fixture.workspace_id,
            "gmail",
            "msg-9",
            "Re: booking",
            &[crowdrelay_domain::drive_contacts::ExtractedContact {
                email: "venue@klubq.pl".to_owned(),
                display_name: Some("Klub Q".to_owned()),
                organization: None,
                phone: None,
                suggested_kind: None,
                city: None,
                notes: None,
            }],
            false,
        )
        .await?;
    let row = sqlx::query_as::<_, (Option<String>, Vec<String>)>(
        "SELECT city, sources FROM drive_contacts \
         WHERE workspace_id = $1 AND normalized_email = 'venue@klubq.pl'",
    )
    .bind(fixture.workspace_id)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(row.0.as_deref(), Some("Wroclaw"));
    let mut sources = row.1;
    sources.sort_unstable();
    assert_eq!(sources, ["gdrive".to_owned(), "gmail".to_owned()]);
    Ok(())
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
        FROM booking_candidates
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
        sqlx::query_scalar("SELECT beacon_outcome FROM drive_contacts WHERE id = $1")
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
        "SELECT (SELECT count(*) FROM booking_candidates WHERE workspace_id = $1), \
         (SELECT beacon_outcome FROM drive_contacts WHERE id = $2)",
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
        "SELECT target_kind, city_slug FROM booking_candidates \
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
        INSERT INTO booking_candidates (
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
        "SELECT count(*) OVER (), (SELECT display_name FROM booking_candidates \
         WHERE workspace_id = $1 AND route_value = 'dup@room.pl') \
         FROM booking_candidates WHERE workspace_id = $1 AND route_value = 'dup@room.pl'",
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
        sqlx::query_scalar("SELECT beacon_outcome FROM drive_contacts WHERE id = $1")
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
        INSERT INTO booking_candidates (
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
        "SELECT city_slug FROM booking_candidates          WHERE workspace_id = $1 AND route_value = 'fill@promo.pl'",
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
        INSERT INTO booking_candidates (
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
        sqlx::query_scalar("SELECT beacon_outcome FROM drive_contacts WHERE id = $1")
            .bind(contact.id)
            .fetch_one(&fixture.pool)
            .await?;
    assert_eq!(staged, "staged");
    Ok(())
}

/// P.2: a sheet row stops being a stranger when the shared registries know
/// the room or the person. The join is the read's own — a venue matches by
/// its key inside the city the sheet named, or by being the only room
/// anywhere carrying that name; a counterparty matches on the address.
#[tokio::test]
#[ignore = "requires a disposable postgres database"]
async fn staged_rows_resolve_against_the_shared_registries()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("registry").await?;
    let wroclaw: Uuid = sqlx::query_scalar("SELECT id FROM cities WHERE slug = 'wroclaw'")
        .fetch_one(&fixture.pool)
        .await?;
    sqlx::query(
        "INSERT INTO cities (id, slug, name, country_code) \
         VALUES (gen_random_uuid(), 'krakow', 'Krakow', 'PL') \
         ON CONFLICT (country_code, slug) DO NOTHING",
    )
    .execute(&fixture.pool)
    .await?;
    let krakow: Uuid = sqlx::query_scalar("SELECT id FROM cities WHERE slug = 'krakow'")
        .fetch_one(&fixture.pool)
        .await?;

    // A published show marks the room and the counterparty through the
    // registry triggers — the band's own evidence, not a hand-seeded row.
    sqlx::query(
        r#"
        INSERT INTO events (
            id, workspace_id, city_id, slug, title, venue,
            counterparty_name, counterparty_email,
            starts_at, status, published_at
        ) VALUES (
            gen_random_uuid(), $1, $2, 'registry-mark-show', 'Played night',
            'Klub Stodola', 'Aga Nowak', 'aga@agency.pl',
            now() - interval '30 days', 'published', now() - interval '30 days'
        )
        "#,
    )
    .bind(fixture.workspace_id)
    .bind(wroclaw)
    .execute(&fixture.pool)
    .await?;

    // A same-named room in another city makes 'Klub Stodola' ambiguous —
    // a cityless row must not guess between the two. The disposable
    // database persists between runs, so the seed tolerates its own echo.
    sqlx::query(
        "INSERT INTO place_venues (city_id, name_key, display_name) \
         VALUES ($1, place_venue_key('Klub Stodola'), 'Klub Stodola') \
         ON CONFLICT (city_id, name_key) DO NOTHING",
    )
    .bind(krakow)
    .execute(&fixture.pool)
    .await?;

    // A counterparty the band never dealt with — on record through another
    // tenant's marks, so the row exists but dealt_with stays false.
    sqlx::query(
        "INSERT INTO place_counterparties (email_key, display_name) \
         VALUES ('known@agency.pl', 'Known Booker') \
         ON CONFLICT (email_key) DO NOTHING",
    )
    .execute(&fixture.pool)
    .await?;

    // P.6 — a second tenant's outreach ledger holds a positive thread on
    // Aga's address, so the prior counts across workspaces, not just this
    // band's own marks. The disposable database persists between runs and
    // each run's workspace is new, so the assertions floor rather than pin.
    let other_tenant = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Other tenant')")
        .bind(other_tenant)
        .bind(format!("registry-other-{}", other_tenant.simple()))
        .execute(&fixture.pool)
        .await?;
    sqlx::query(
        "INSERT INTO outreach_targets
            (workspace_id, target_kind, display_name, contact_email,
             last_outreach_at, last_reply_at, last_reply_disposition)
         VALUES ($1, 'support_slot', 'Aga Nowak', 'aga@agency.pl',
                 now() - interval '40 days', now() - interval '39 days', 'positive')",
    )
    .bind(other_tenant)
    .execute(&fixture.pool)
    .await?;
    sqlx::query(
        "INSERT INTO outreach_interactions
            (workspace_id, target_id, direction, phase, disposition, source_key, occurred_at)
         SELECT workspace_id, id, 'inbound', 'reply', 'positive',
                'seed-aga-reply', now() - interval '39 days'
         FROM outreach_targets
         WHERE workspace_id = $1 AND contact_email = 'aga@agency.pl'",
    )
    .bind(other_tenant)
    .execute(&fixture.pool)
    .await?;

    // The sheet.
    seed_contact(&fixture, "bookings@stodola.pl", Some("Klub Stodola"), None).await?;
    let own_room = seed_contact_city(
        &fixture,
        "produk@stodola.pl",
        Some("Stodola production"),
        Some("wroclaw"),
    )
    .await?;
    sqlx::query("UPDATE drive_contacts SET organization = 'Klub Stodola' WHERE id = $1")
        .bind(own_room.id)
        .execute(&fixture.pool)
        .await?;
    seed_contact(&fixture, "aga@agency.pl", Some("Aga Nowak"), None).await?;
    seed_contact(&fixture, "known@agency.pl", Some("Known Booker"), None).await?;
    seed_contact(
        &fixture,
        "stranger@elsewhere.pl",
        Some("Stranger"),
        Some("Nowhere Inn"),
    )
    .await?;
    // The ambiguous name, no city — must not guess.
    let ambiguous =
        seed_contact_city(&fixture, "info@stodola.pl", Some("Klub Stodola"), None).await?;
    sqlx::query("UPDATE drive_contacts SET organization = 'Klub Stodola' WHERE id = $1")
        .bind(ambiguous.id)
        .execute(&fixture.pool)
        .await?;

    let rows = fixture
        .repository
        .list_contacts(fixture.workspace_id, 50)
        .await?;
    let by_email = |email: &str| {
        rows.iter()
            .find(|v| v.row.normalized_email == email)
            .unwrap()
    };

    // City-placed match: org keys to the wroclaw room, own marks say played.
    let own = by_email("produk@stodola.pl");
    assert_eq!(own.row.matched_venue.as_deref(), Some("Klub Stodola"));
    assert!(own.row.venue_played_here);
    // P.6 — this tenant's own mark is part of the room's record: the prior
    // floors at one tenant, one show.
    let own_prior = own.venue_prior.expect("a matched venue carries its prior");
    assert!(
        own_prior.tenants_played >= 1 && own_prior.shows >= 1,
        "Klub Stodola's marks are the shared record: {own_prior:?}"
    );

    // The ambiguous name with no city resolves to nothing — guessing would
    // file the row against the wrong room.
    let ambiguous = by_email("info@stodola.pl");
    assert_eq!(ambiguous.row.matched_venue, None);

    // The played counterparty and the merely-known one stay distinct.
    let played = by_email("aga@agency.pl");
    assert_eq!(
        played.row.matched_counterparty.as_deref(),
        Some("Aga Nowak")
    );
    assert!(played.row.counterparty_worked_with);
    // P.6 — the shared reply record rides the row: at least this run's
    // other-tenant thread answered and won. Only counts, never names.
    let played_prior = played
        .counterparty_prior
        .expect("the prior read ran for the list");
    assert!(
        played_prior.tenants_contacted >= 1
            && played_prior.tenants_replied >= 1
            && played_prior.tenants_won >= 1,
        "aga@agency.pl carries the other tenant's won thread: {played_prior:?}"
    );
    let known = by_email("known@agency.pl");
    assert_eq!(
        known.row.matched_counterparty.as_deref(),
        Some("Known Booker")
    );
    assert!(!known.row.counterparty_worked_with);

    // The stranger stays a stranger — registry-blind, and the prior read
    // still ran: a measured zero is an answer, not an absence.
    let stranger = by_email("stranger@elsewhere.pl");
    assert_eq!(stranger.row.matched_venue, None);
    assert_eq!(stranger.row.matched_counterparty, None);
    assert!(!stranger.row.venue_played_here);
    assert!(!stranger.row.counterparty_worked_with);
    assert_eq!(
        stranger.counterparty_prior.map(|prior| (
            prior.tenants_contacted,
            prior.tenants_replied,
            prior.tenants_won
        )),
        Some((0, 0, 0)),
        "no ledger anywhere names the stranger's address"
    );

    let summary = fixture
        .repository
        .registry_summary(fixture.workspace_id)
        .await?;
    assert_eq!(summary.total, 6);
    assert_eq!(summary.known_venues, 1);
    assert_eq!(summary.own_rooms, 1);
    assert_eq!(summary.known_counterparties, 2);
    assert_eq!(summary.dealt_with, 1);
    Ok(())
}

/// P.2: an operator's pasted sheet stages like a connector's sighting —
/// 'upload' is a third lawful source, and a second upload of the same file
/// name refreshes rather than doubling.
#[tokio::test]
#[ignore = "requires a disposable postgres database"]
async fn uploaded_sheet_stages_through_the_upload_source() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = fixture("upload").await?;
    // The address the sheet will carry was already sighted over Gmail —
    // the upload must converge to one row carrying both sources.
    sqlx::query(
        "INSERT INTO drive_contacts \
            (workspace_id, normalized_email, source_file_id, source_file_name, sources) \
         VALUES ($1, 'first@sheet.test', 'msg-9', 'Re: hello', '{gmail}')",
    )
    .bind(fixture.workspace_id)
    .execute(&fixture.pool)
    .await?;
    let contacts = vec![
        crowdrelay_domain::drive_contacts::ExtractedContact {
            email: "first@sheet.test".to_owned(),
            display_name: Some("First".to_owned()),
            organization: None,
            phone: None,
            suggested_kind: None,
            city: Some("wroclaw".to_owned()),
            notes: None,
        },
        crowdrelay_domain::drive_contacts::ExtractedContact {
            email: "second@sheet.test".to_owned(),
            display_name: None,
            organization: Some("Agency Y".to_owned()),
            phone: None,
            suggested_kind: Some("promoter".to_owned()),
            city: None,
            notes: None,
        },
    ];
    let summary = fixture
        .repository
        .upsert_contacts_for_source(
            fixture.workspace_id,
            "upload",
            "upload:sheet.csv",
            "sheet.csv",
            &contacts,
            false,
        )
        .await?;
    assert_eq!(summary.upserted, 2);

    // The widened CHECK accepts 'upload', and the rows land staged.
    let staged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM drive_contacts \
         WHERE workspace_id = $1 AND 'upload' = ANY(sources)",
    )
    .bind(fixture.workspace_id)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(staged, 2);

    // A re-upload of the same file refreshes rather than doubling, and
    // the gmail+upload union survives it.
    fixture
        .repository
        .upsert_contacts_for_source(
            fixture.workspace_id,
            "upload",
            "upload:sheet.csv",
            "sheet.csv",
            &contacts,
            false,
        )
        .await?;
    let sources: Vec<String> = sqlx::query_scalar(
        "SELECT sources FROM drive_contacts \
         WHERE workspace_id = $1 AND normalized_email = 'first@sheet.test'",
    )
    .bind(fixture.workspace_id)
    .fetch_one(&fixture.pool)
    .await?;
    assert!(sources.contains(&"upload".to_owned()));
    assert!(sources.contains(&"gmail".to_owned()));
    Ok(())
}
