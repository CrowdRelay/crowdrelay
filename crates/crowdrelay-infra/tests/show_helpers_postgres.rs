//! §4h-11 — the staging queue read against a date, and the city a press
//! contact keeps when it is promoted (3.9, 3.10).
//!
//! Both halves are database shape rather than policy: a column that must carry
//! through a promote, and a read that must place contacts in a city and say
//! honestly how many it could not place. SQLx checks none of that at compile
//! time, so it is driven here.

use std::time::Duration;

use crowdrelay_infra::show_helpers::{HelperState, who_can_help};
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_show_names_the_people_who_could_fill_the_room() -> Result<(), Box<dyn std::error::Error>>
{
    let database = DisposableDatabase::create().await?;
    let result = run(&database.pool).await;
    database.drop_database().await;
    result
}

async fn run(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let act = workspace(pool).await?;
    let wroclaw = city(pool, "wroclaw", "Wrocław", 51.1, 17.03).await?;
    let krakow = city(pool, "krakow", "Kraków", 50.06, 19.94).await?;
    show(pool, act, wroclaw, "friday").await?;
    show(pool, act, krakow, "next-month").await?;

    // The local paper, promoted and contactable.
    press(
        pool,
        act,
        Some(wroclaw),
        "Gazeta",
        "press",
        "promoted",
        true,
        false,
    )
    .await?;
    // A staged one the archive scan produced — the larger half in practice,
    // and the one no other screen shows.
    press(
        pool,
        act,
        Some(wroclaw),
        "Radio Nowe",
        "radio",
        "proposed",
        false,
        false,
    )
    .await?;
    // Promoted, and they asked not to be contacted. Reported as blocked with
    // the reason rather than filtered away: a band that cannot see why
    // somebody is missing asks for them again next month.
    press(
        pool,
        act,
        Some(wroclaw),
        "Zine",
        "press",
        "promoted",
        true,
        true,
    )
    .await?;
    // A national title with no city. Counted, never invented into a city.
    press(pool, act, None, "Krajowy", "press", "promoted", true, false).await?;
    // Another city's paper must not appear on this night.
    press(
        pool,
        act,
        Some(krakow),
        "Kraków Daily",
        "press",
        "promoted",
        true,
        false,
    )
    .await?;

    booking_target(pool, act, wroclaw, "Anna", "promoter", true, true).await?;
    booking_target(pool, act, wroclaw, "Stary Klub", "venue", false, true).await?;
    booking_candidate(pool, act, "wroclaw", "Nowy Klub", "venue").await?;

    let helpers = who_can_help(pool, act, "friday")
        .await?
        .ok_or("the show produced no helper read")?;
    assert_eq!(helpers.city, "Wrocław");

    let voice = |name: &str| {
        helpers
            .voices
            .iter()
            .find(|helper| helper.display_name == name)
            .cloned()
    };
    assert_eq!(
        helpers.voices.len(),
        3,
        "the city's voices are the three placed here, got {:?}",
        helpers
            .voices
            .iter()
            .map(|helper| helper.display_name.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        voice("Gazeta").ok_or("the local paper is missing")?.state,
        HelperState::Ready
    );
    // Contactable first. The status column sorts 'promoted' before 'proposed',
    // so ordering by it descending put every staged row above every ready one
    // — and the limit is applied after the sort, so a workspace with forty
    // staged contacts would have seen none it could write to.
    assert_eq!(
        helpers.voices[0].state,
        HelperState::Ready,
        "a staged contact outranked one that can be written to: {:?}",
        helpers
            .voices
            .iter()
            .map(|helper| (helper.display_name.as_str(), helper.state))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        voice("Radio Nowe")
            .ok_or("the staged station is missing")?
            .state,
        HelperState::NeedsReview
    );
    let blocked = voice("Zine").ok_or("the do-not-contact title is missing")?;
    assert_eq!(blocked.state, HelperState::Blocked);
    assert!(
        blocked
            .blocked_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("not to be contacted")),
        "the block did not say which rule: {:?}",
        blocked.blocked_reason
    );
    assert!(
        voice("Kraków Daily").is_none(),
        "another city's paper appeared on this night"
    );
    assert_eq!(
        helpers.contacts_unplaced, 1,
        "the national title was not counted as unplaced"
    );
    // A community is an outreach target carrying a place, and it can never be
    // placed in a city — the place graph records a country and nothing finer.
    // The first version of this count asked `discovery_places` for a status
    // its CHECK does not allow, so it could only ever answer zero: a measured
    // absence that was really an unasked question.
    community(pool, act, "r/wroclaw").await?;
    community(pool, act, "r/polishmetal").await?;
    let with_communities = who_can_help(pool, act, "friday")
        .await?
        .ok_or("the show produced no helper read")?;
    assert_eq!(
        with_communities.communities_unplaced, 2,
        "communities the workspace holds were not counted"
    );
    assert_eq!(
        with_communities.contacts_unplaced, 1,
        "a community was counted as an unplaced press contact as well"
    );

    let booker = |name: &str| {
        helpers
            .bookers
            .iter()
            .find(|helper| helper.display_name == name)
            .cloned()
    };
    assert_eq!(
        booker("Anna").ok_or("the promoter is missing")?.state,
        HelperState::Ready
    );
    assert_eq!(
        booker("Stary Klub")
            .ok_or("the inactive room is missing")?
            .state,
        HelperState::Blocked
    );
    assert_eq!(
        booker("Nowy Klub")
            .ok_or("the staged room is missing")?
            .state,
        HelperState::NeedsReview
    );

    // A show in the other city answers with that city's people, which is the
    // whole point of reading the queue against a date.
    let other = who_can_help(pool, act, "next-month")
        .await?
        .ok_or("the second show produced no helper read")?;
    assert_eq!(other.city, "Kraków");
    assert_eq!(
        other
            .voices
            .iter()
            .map(|helper| helper.display_name.as_str())
            .collect::<Vec<_>>(),
        vec!["Kraków Daily"]
    );

    // An event nobody has is not an empty list.
    assert!(who_can_help(pool, act, "no-such-show").await?.is_none());

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
    let gdansk = city(pool, "gdansk", "Gdańsk", 54.35, 18.65).await?;
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
    city(pool, "springfield", "Springfield", 39.79, -89.64).await?;
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
    latitude: f64,
    longitude: f64,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ($1, $2, 'PL', $3, $4)
         ON CONFLICT (country_code, slug) DO UPDATE SET name = EXCLUDED.name
         RETURNING id",
    )
    .bind(slug)
    .bind(name)
    .bind(latitude)
    .bind(longitude)
    .fetch_one(pool)
    .await?)
}

async fn show(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    slug: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO events
            (workspace_id, city_id, slug, title, venue, starts_at, status, published_at)
         VALUES ($1, $2, $3, $3, 'Klub X', now() + interval '20 days', 'published', now())",
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(slug)
    .execute(pool)
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn press(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Option<Uuid>,
    display_name: &str,
    kind: &str,
    status: &str,
    accepts_outreach: bool,
    do_not_contact: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO agent_outreach_targets
            (workspace_id, target_kind, display_name, contact_email, status,
             accepts_outreach, do_not_contact, city_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(workspace_id)
    .bind(kind)
    .bind(display_name)
    .bind(format!(
        "{}@example.com",
        display_name.to_lowercase().replace(' ', "-")
    ))
    .bind(status)
    .bind(accepts_outreach)
    .bind(do_not_contact)
    .bind(city_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// A community the workspace has on file: an outreach target attached to a
/// place. Placeless by construction — `discovery_places` records a country.
async fn community(
    pool: &PgPool,
    workspace_id: Uuid,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let place = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO discovery_places
            (workspace_id, place_kind, platform, name, url, country_code, status)
         VALUES ($1, 'subreddit', 'reddit', $2, $3, 'PL', 'active')
         RETURNING id",
    )
    .bind(workspace_id)
    .bind(name)
    .bind(format!("https://www.reddit.com/{name}"))
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO agent_outreach_targets
            (workspace_id, target_kind, display_name, status, place_id,
             screening_verdict)
         VALUES ($1, 'media_patronage', $2, 'promoted', $3, 'admitted')",
    )
    .bind(workspace_id)
    .bind(name)
    .bind(place)
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
    accepts_booking: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO viryaos_booking_targets
            (workspace_id, city_id, target_kind, display_name, contact_email,
             relationship_score, active, accepts_booking)
         VALUES ($1, $2, $3, $4, $5, 60, $6, $7)",
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
    .bind(accepts_booking)
    .execute(pool)
    .await?;
    Ok(())
}

async fn booking_candidate(
    pool: &PgPool,
    workspace_id: Uuid,
    city_slug: &str,
    display_name: &str,
    kind: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO viryaos_booking_candidates
            (workspace_id, target_kind, display_name, city_slug, route_kind,
             route_value, source, source_reference, fit_basis_points, status)
         VALUES ($1, $2, $3, $4, 'email', $5, 'contact_scan', 'sheet', 6000, 'admitted')",
    )
    .bind(workspace_id)
    .bind(kind)
    .bind(display_name)
    .bind(city_slug)
    .bind(format!(
        "{}@example.com",
        display_name.to_lowercase().replace(' ', "-")
    ))
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
