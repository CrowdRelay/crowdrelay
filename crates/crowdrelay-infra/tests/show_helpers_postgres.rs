//! §4h-11 — the staging queue read against a date, and the city a press
//! contact keeps when it is promoted (3.9, 3.10).
//!
//! Both halves are database shape rather than policy: a column that must carry
//! through a promote, and a read that must place candidates in a city while
//! scoping each tenant to its own — plus the one deliberate global read, the
//! cold rooms any tenant may see. SQLx checks none of that at compile time,
//! so it is driven here.

use std::time::Duration;

use crowdrelay_infra::show_helpers::who_can_help;
use sqlx::{Connection, PgConnection, PgPool, postgres::PgPoolOptions};
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
        let name = format!("crowdrelay_helpers_{}", Uuid::now_v7().simple());
        let mut admin = PgConnection::connect(&base).await?;
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await?;
        drop(admin);
        let (head, _) = base.rsplit_once('/').ok_or("database url has no path")?;
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(10))
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

/// 3.10 — two workspaces seed different candidates for the same city; each
/// sees only its own plus the shared cold rooms; and an event with no city
/// degrades rather than erroring.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn each_workspace_sees_its_own_candidates_and_the_shared_cold_rooms()
-> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = run(&database.pool).await;
    database.drop_database().await;
    result
}

async fn run(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let act = workspace(pool).await?;
    let other = workspace(pool).await?;
    let wroclaw = city(pool, "wroclaw", "Wrocław", "PL", 51.1, 17.03).await?;
    let krakow = city(pool, "krakow", "Kraków", "PL", 50.06, 19.94).await?;
    let berlin = city(pool, "berlin", "Berlin", "DE", 52.52, 13.4).await?;
    // The show's own venue mark lands via the event trigger — "Klub A" is a
    // room this tenant has played, so it must never list as cold.
    show(pool, act, Some(wroclaw), "friday", "Klub A").await?;
    show(pool, other, Some(wroclaw), "friday-too", "Klub B").await?;
    show(pool, act, None, "no-city-yet", "TBA").await?;
    show(pool, act, Some(wroclaw), "draft-show", "Klub A").await?;
    sqlx::query("UPDATE events SET status = 'draft' WHERE slug = 'draft-show'")
        .execute(pool)
        .await?;

    // ── press: proposed only, this workspace, this city, media kinds ──
    press(pool, act, Some(wroclaw), "Gazeta", "press", "proposed").await?;
    // Promoted contacts are already past the candidate stage — the promote
    // flow owns them, so the shortlist does not repeat them.
    press(pool, act, Some(wroclaw), "Old Paper", "press", "promoted").await?;
    // Another city's paper must not appear on this night.
    press(pool, act, Some(krakow), "Kraków Daily", "press", "proposed").await?;
    // An endorsement target is not press — the kind list is the media set.
    press(
        pool,
        act,
        Some(wroclaw),
        "Sponsor Lady",
        "endorsement",
        "proposed",
    )
    .await?;
    // The other workspace's candidate for the same city stays its own.
    press(
        pool,
        other,
        Some(wroclaw),
        "Rival Radio",
        "radio",
        "proposed",
    )
    .await?;

    // ── rooms_and_promoters: active only, this workspace, this city ──
    booking_target(pool, act, wroclaw, "Anna", "promoter", true).await?;
    booking_target(pool, act, wroclaw, "Dead Club", "venue", false).await?;
    booking_target(pool, other, wroclaw, "Their Booker", "promoter", true).await?;

    // ── communities: this workspace, the city's country, active ──
    community(pool, act, "PL", "r/wroclaw", true).await?;
    community(pool, act, "PL", "r/inactive", false).await?;
    community(pool, act, "DE", "r/berlin", true).await?;
    community(pool, other, "PL", "r/theirs", true).await?;

    // ── cold_rooms: the registry's rooms minus this tenant's marks ──
    // "Klub A" (marked by the show above) and "Klub B" (marked by the other
    // workspace) already exist in place_venues through the trigger. Two more
    // are rooms nobody has marked — one with a public capacity fact, one
    // with none, and a private fact on the first that must never leak.
    let cold = venue(pool, wroclaw, "Cold Klub").await?;
    let no_capacity = venue(pool, wroclaw, "No Cap Klub").await?;
    venue(pool, berlin, "Berlin Cold").await?;
    venue_fact(pool, cold, "capacity", "350", None).await?;
    venue_fact(pool, no_capacity, "capacity", "999", Some(other)).await?;

    let helpers = who_can_help(pool, act, "friday")
        .await?
        .ok_or("the show produced no helper read")?;
    assert_eq!(helpers.event.slug, "friday");
    assert_eq!(helpers.event.city.as_deref(), Some("Wrocław"));
    assert_eq!(helpers.event.country_code.as_deref(), Some("PL"));
    assert!(
        helpers.degraded.is_empty(),
        "degraded: {:?}",
        helpers.degraded
    );
    assert_eq!(helpers.notes, ["staged_contacts_have_no_city"]);

    // press — only the city's proposed media contact, nothing promoted,
    // nothing another city's, nothing that is not a media kind, nothing the
    // other workspace holds.
    let names: Vec<&str> = helpers
        .press
        .iter()
        .map(|row| row.display_name.as_str())
        .collect();
    assert_eq!(names, ["Gazeta"], "press section: {names:?}");
    assert_eq!(helpers.press[0].target_kind, "press");
    // No contact_email field exists on the row at all — the type carries no
    // address, which is the assertion that cannot drift.

    let rooms: Vec<&str> = helpers
        .rooms_and_promoters
        .iter()
        .map(|row| row.display_name.as_str())
        .collect();
    assert_eq!(
        rooms,
        ["Anna"],
        "rooms_and_promoters must hold only this tenant's active targets: {rooms:?}"
    );

    let communities: Vec<(&str, &str)> = helpers
        .communities
        .iter()
        .map(|row| (row.community_name.as_str(), row.country.as_str()))
        .collect();
    assert_eq!(
        communities,
        [("r/wroclaw", "PL")],
        "communities must be this tenant's, active, in the show's country: {communities:?}"
    );

    // cold_rooms — the shared registry minus this tenant's marks. "Klub A"
    // is out because the tenant played it; "Klub B" is IN because the other
    // workspace's mark is not this tenant's knowledge. The capacity fact
    // surfaces globally; the private fact on "No Cap Klub" must not.
    let cold_names: Vec<&str> = helpers
        .cold_rooms
        .iter()
        .map(|row| row.display_name.as_str())
        .collect();
    assert_eq!(
        cold_names.first(),
        Some(&"Cold Klub"),
        "a room with a known capacity sorts first: {cold_names:?}"
    );
    assert_eq!(helpers.cold_rooms[0].capacity.as_deref(), Some("350"));
    assert!(
        cold_names.contains(&"Klub B"),
        "a room only the other workspace played is cold to this one: {cold_names:?}"
    );
    assert!(
        cold_names.contains(&"No Cap Klub"),
        "an unmarked room with no capacity fact still lists: {cold_names:?}"
    );
    assert!(
        !cold_names.contains(&"Klub A"),
        "a room this tenant played is not cold: {cold_names:?}"
    );
    assert!(
        !cold_names.contains(&"Berlin Cold"),
        "another city's room is not on this night: {cold_names:?}"
    );
    let no_cap = helpers
        .cold_rooms
        .iter()
        .find(|row| row.display_name == "No Cap Klub")
        .ok_or("No Cap Klub is missing")?;
    assert_eq!(
        no_cap.capacity, None,
        "the other workspace's private fact leaked into a global read"
    );

    // The other workspace's read of the same city: its own press, its own
    // booker, its own community — and the cold rooms flip: "Klub A" is cold
    // to it, "Klub B" is not.
    let theirs = who_can_help(pool, other, "friday-too")
        .await?
        .ok_or("the second show produced no helper read")?;
    let their_press: Vec<&str> = theirs
        .press
        .iter()
        .map(|row| row.display_name.as_str())
        .collect();
    assert_eq!(their_press, ["Rival Radio"], "their press: {their_press:?}");
    let their_cold: Vec<&str> = theirs
        .cold_rooms
        .iter()
        .map(|row| row.display_name.as_str())
        .collect();
    assert!(
        their_cold.contains(&"Klub A") && !their_cold.contains(&"Klub B"),
        "cold rooms did not flip per workspace: {their_cold:?}"
    );

    // A show with no city degrades: every section empty, "city" named, never
    // an error — a band needs to see the missing city, not a 404.
    let no_city = who_can_help(pool, act, "no-city-yet")
        .await?
        .ok_or("a city-less show produced no read")?;
    assert_eq!(no_city.degraded, ["city"]);
    assert_eq!(no_city.event.city, None);
    assert!(no_city.press.is_empty());
    assert!(no_city.rooms_and_promoters.is_empty());
    assert!(no_city.communities.is_empty());
    assert!(no_city.cold_rooms.is_empty());
    assert_eq!(no_city.notes, ["staged_contacts_have_no_city"]);

    // An event nobody has is not an empty list — and neither is a draft,
    // matching the timeline's resolution exactly.
    assert!(who_can_help(pool, act, "no-such-show").await?.is_none());
    assert!(who_can_help(pool, act, "draft-show").await?.is_none());

    a_promoted_press_contact_keeps_its_city(pool).await?;

    Ok(())
}

/// 3.9 — the city the sheet placed a contact in survives the promote.
///
/// Before this, `agent_outreach_targets` had no city at all: a band playing a
/// city could not ask which of its own press contacts were in it, and the
/// staged city was dropped on the way through.
async fn a_promoted_press_contact_keeps_its_city(
    pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let act = workspace(pool).await?;
    let gdansk = city(pool, "gdansk", "Gdańsk", "PL", 54.35, 18.65).await?;
    let repository = crowdrelay_infra::gdrive::PostgresGDriveRepository::new(pool.clone());

    // The sheet wrote the city in its own words, with its own diacritics.
    let staged = staged_contact(pool, act, "editor@example.com", Some("Gdańsk")).await?;
    let contact = repository.get_contact(act, staged).await?;
    repository.promote_beacon(act, &contact, "press").await?;
    let city_id = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT city_id FROM agent_outreach_targets
         WHERE workspace_id = $1 AND contact_email = 'editor@example.com'",
    )
    .bind(act)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        city_id,
        Some(gdansk),
        "the staged city did not survive the promote"
    );

    // A contact the sheet placed nowhere promotes with no city rather than
    // being refused — a national title is legitimately placeless, and the
    // booking half's city requirement does not apply here.
    let placeless = staged_contact(pool, act, "national@example.com", None).await?;
    let contact = repository.get_contact(act, placeless).await?;
    repository.promote_beacon(act, &contact, "press").await?;
    let city_id = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT city_id FROM agent_outreach_targets
         WHERE workspace_id = $1 AND contact_email = 'national@example.com'",
    )
    .bind(act)
    .fetch_one(pool)
    .await?;
    assert_eq!(city_id, None, "a placeless contact was given a city");

    // A city name two catalogue rows share resolves to nothing rather than to
    // whichever came first — the booking half's rule, for the same reason:
    // a contact filed in the wrong city is suggested for the wrong shows.
    city(pool, "springfield", "Springfield", "US", 39.79, -89.64).await?;
    sqlx::query(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ('springfield-ma', 'Springfield', 'US', 42.1, -72.59)",
    )
    .execute(pool)
    .await?;
    let ambiguous = staged_contact(pool, act, "ambiguous@example.com", Some("Springfield")).await?;
    let contact = repository.get_contact(act, ambiguous).await?;
    repository.promote_beacon(act, &contact, "press").await?;
    let city_id = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT city_id FROM agent_outreach_targets
         WHERE workspace_id = $1 AND contact_email = 'ambiguous@example.com'",
    )
    .bind(act)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        city_id, None,
        "an ambiguous city name was resolved to one of the candidates anyway"
    );

    Ok(())
}

async fn workspace(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Helpers Test')")
        .bind(id)
        .bind(format!("ws-{}", id.simple()))
        .execute(pool)
        .await?;
    Ok(id)
}

async fn city(
    pool: &PgPool,
    slug: &str,
    name: &str,
    country_code: &str,
    latitude: f64,
    longitude: f64,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (country_code, slug) DO UPDATE SET name = EXCLUDED.name
         RETURNING id",
    )
    .bind(slug)
    .bind(name)
    .bind(country_code)
    .bind(latitude)
    .bind(longitude)
    .fetch_one(pool)
    .await?)
}

/// A show row. `venue` names the room so the registry trigger marks it —
/// a published or completed event with a venue and a city is what feeds
/// `place_venues`/`place_venue_marks`, and that is exactly what makes the
/// room warm rather than cold for this tenant.
async fn show(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Option<Uuid>,
    slug: &str,
    venue: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO events
            (workspace_id, city_id, slug, title, venue, starts_at, status, published_at)
         VALUES ($1, $2, $3, $3, $4, now() + interval '20 days', 'published', now())",
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(slug)
    .bind(venue)
    .execute(pool)
    .await?;
    Ok(())
}

async fn press(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Option<Uuid>,
    display_name: &str,
    kind: &str,
    status: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO agent_outreach_targets
            (workspace_id, target_kind, display_name, contact_email, status, city_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(workspace_id)
    .bind(kind)
    .bind(display_name)
    .bind(format!(
        "{}@example.com",
        display_name.to_lowercase().replace(' ', "-")
    ))
    .bind(status)
    .bind(city_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// A community outreach target — the communities section's source table.
/// `country_code` is all the placement it carries; there is no city.
async fn community(
    pool: &PgPool,
    workspace_id: Uuid,
    country_code: &str,
    name: &str,
    active: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO viryaos_community_outreach_targets
            (workspace_id, symbol_slug, community_name, platform, url,
             country_code, active)
         VALUES ($1, $2, $3, 'reddit', $4, $5, $6)",
    )
    .bind(workspace_id)
    .bind(name.replace('/', "-"))
    .bind(name)
    .bind(format!("https://www.reddit.com/{name}"))
    .bind(country_code)
    .bind(active)
    .execute(pool)
    .await?;
    Ok(())
}

async fn booking_target(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    display_name: &str,
    kind: &str,
    active: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO viryaos_booking_targets
            (workspace_id, city_id, target_kind, display_name, contact_email,
             relationship_score, active)
         VALUES ($1, $2, $3, $4, $5, 60, $6)",
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(kind)
    .bind(display_name)
    .bind(format!(
        "{}@example.com",
        display_name.to_lowercase().replace(' ', "-")
    ))
    .bind(active)
    .execute(pool)
    .await?;
    Ok(())
}

/// A registry row directly — the rooms nobody has marked, which is what
/// makes them cold to every workspace rather than just this one.
async fn venue(
    pool: &PgPool,
    city_id: Uuid,
    display_name: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO place_venues (city_id, name_key, display_name)
         VALUES ($1, place_venue_key($2), $2)
         RETURNING id",
    )
    .bind(city_id)
    .bind(display_name)
    .fetch_one(pool)
    .await?)
}

/// A fact about a room — `None` workspace writes the global record any
/// tenant may read, `Some` writes a private one the global read must skip.
async fn venue_fact(
    pool: &PgPool,
    venue_id: Uuid,
    attribute: &str,
    value: &str,
    workspace_id: Option<Uuid>,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO place_venue_facts
            (venue_id, attribute, value, provenance, source_ref, observed_at, workspace_id)
         VALUES ($1, $2, $3, 'researched', 'test', now(), $4)",
    )
    .bind(venue_id)
    .bind(attribute)
    .bind(value)
    .bind(workspace_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn staged_contact(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
    city: Option<&str>,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO viryaos_drive_contacts
            (workspace_id, normalized_email, display_name, city,
             source_file_id, source_file_name, suggested_kind)
         VALUES ($1, $2, $3, $4, 'file-1', 'contacts.xlsx', 'press')
         RETURNING id",
    )
    .bind(workspace_id)
    .bind(email)
    .bind(email.split('@').next().unwrap_or("contact"))
    .bind(city)
    .fetch_one(pool)
    .await?)
}
