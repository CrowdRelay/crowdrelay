//! The venue-seed intake: a researched sheet's rows must land in the shared
//! registry with every claim attributed — the room's facts global, the
//! researcher's private knowledge scoped to the workspace that wrote it.
//!
//! Migration 0308 is the first writer `place_venue_facts` has, so the only
//! way to know the split holds is to drive the table. Each case below is a
//! way the import was wrong at some point: a global fact carrying a
//! workspace, a booking address leaking into the shared half, a re-scan
//! stacking a duplicate instead of refreshing the claim, and a room whose
//! city is not in the catalogue being filed as a guess.

use crowdrelay_domain::venue_seed::{
    PublicTerms, SeedSheetReport, SeededRoom, SeededRoomView, SeededVenue,
};
use crowdrelay_infra::venue_seed::PostgresVenueSeedRepository;
use sqlx::{Connection, PgConnection, PgPool, Row, postgres::PgPoolOptions};
use time::{Date, Month};
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
        let name = format!("crowdrelay_venueseed_{}", Uuid::now_v7().simple());
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

/// A row the way `domain::venue_seed::parse_seed_row` would produce it.
fn venue(
    name: &str,
    city: &str,
    room: impl FnOnce(&mut SeededRoom),
    view: impl FnOnce(&mut SeededRoomView),
) -> SeededVenue {
    let mut seeded = SeededRoom {
        name: name.to_owned(),
        city: city.to_owned(),
        country: String::new(),
        address: None,
        website: None,
        genre_tags: Vec::new(),
        capacity: None,
        booking_email: None,
        public_terms: PublicTerms::NotSearched,
        source_url: format!("https://source.example/{name}"),
        researched_on: Some("2026-09-20".to_owned()),
        closed: false,
    };
    room(&mut seeded);
    let mut seeded_view = SeededRoomView::default();
    view(&mut seeded_view);
    SeededVenue {
        room: seeded,
        view: seeded_view,
    }
}

/// The facts a venue carries, as (attribute, value, workspace_id) — the
/// workspace being NULL is the whole assertion: a NULL here is the global
/// half, the workspace's own id is the private half.
async fn facts(
    pool: &PgPool,
    venue_id: Uuid,
) -> Result<Vec<(String, String, Option<Uuid>)>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT attribute, value, workspace_id FROM place_venue_facts \
         WHERE venue_id = $1 ORDER BY attribute, value",
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
                row.get::<Option<Uuid>, _>("workspace_id"),
            )
        })
        .collect())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_seed_sheet_lands_as_attributed_facts() -> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = run_cases(&database.pool).await;
    database.drop_database().await;
    result
}

async fn run_cases(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = seed_workspace(pool).await?;
    let repo = PostgresVenueSeedRepository::new(pool.clone());

    let report = SeedSheetReport {
        venues: vec![
            // The full row: shared facts, a published rate card, and one
            // tenant's judgement riding alongside. "Wrocław" exercises the
            // name-match path — the catalogue slug is ASCII, the sheet's
            // spelling is not.
            venue(
                "Stodoła",
                "Wrocław",
                |room| {
                    room.country = "Poland".to_owned();
                    room.address = Some("ul. Batorego 10".to_owned());
                    room.website = Some("https://stodola.pl".to_owned());
                    room.genre_tags = vec!["rock".to_owned(), "punk".to_owned()];
                    room.capacity = Some(1500);
                    room.booking_email = Some("klub@stodola.pl".to_owned());
                    room.public_terms =
                        PublicTerms::Published("hire rate card: 1200 EUR".to_owned());
                },
                |view| {
                    view.target_fit = Some("High".to_owned());
                    view.contact_quality = Some("Booking email".to_owned());
                    view.notes = Some("Three stages".to_owned());
                },
            ),
            // A room that was searched and published nothing — that search
            // is the tenant's knowledge, not the world's. "poznan" is the
            // catalogue slug itself, exercising the direct-slug path.
            venue(
                "Klub B",
                "poznan",
                |room| {
                    room.public_terms = PublicTerms::SearchedNoneFound;
                    room.closed = true;
                },
                |_| {},
            ),
            // A city the catalogue does not hold: counted, never guessed.
            venue(
                "Klub C",
                "Nowhereville",
                |room| {
                    room.capacity = Some(300);
                },
                |_| {},
            ),
        ],
        refusals: Vec::new(),
    };

    let summary = repo.import_sheet(workspace, &report).await?;
    assert_eq!(summary.imported, 2);
    assert_eq!(summary.unknown_city, 1);

    // ── The room is one identity row, keyed the way the registry keys it. ──
    let stodola = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM place_venues \
         WHERE name_key = place_venue_key('Stodoła') \
           AND city_id = (SELECT id FROM cities WHERE slug = 'wroclaw')",
    )
    .fetch_one(pool)
    .await?;
    let wroclaw = sqlx::query_scalar::<_, Uuid>("SELECT city_id FROM place_venues WHERE id = $1")
        .bind(stodola)
        .fetch_one(pool)
        .await?;
    assert_eq!(
        wroclaw,
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM cities WHERE slug = 'wroclaw'")
            .fetch_one(pool)
            .await?
    );

    // ── Global facts carry workspace_id NULL — the room as everyone sees it.
    let stodola_facts = facts(pool, stodola).await?;
    let fact = |facts: &[(String, String, Option<Uuid>)], attribute: &str| {
        facts
            .iter()
            .find(|(a, _, _)| a == attribute)
            .map(|(_, v, ws)| (v.clone(), *ws))
    };
    assert_eq!(
        fact(&stodola_facts, "capacity"),
        Some(("1500".to_owned(), None))
    );
    assert_eq!(
        fact(&stodola_facts, "genres"),
        Some(("rock, punk".to_owned(), None))
    );
    assert_eq!(
        fact(&stodola_facts, "website"),
        Some(("https://stodola.pl".to_owned(), None))
    );
    assert_eq!(
        fact(&stodola_facts, "address"),
        Some(("ul. Batorego 10".to_owned(), None))
    );
    assert_eq!(
        fact(&stodola_facts, "country"),
        Some(("Poland".to_owned(), None))
    );

    // The room published the terms itself — the one case a terms fact may
    // be global.
    assert_eq!(
        fact(&stodola_facts, "public_terms"),
        Some(("hire rate card: 1200 EUR".to_owned(), None))
    );

    // ── Contact and judgement are never global. ──────────────────────────
    assert_eq!(
        fact(&stodola_facts, "booking_email"),
        Some(("klub@stodola.pl".to_owned(), Some(workspace)))
    );
    assert_eq!(
        fact(&stodola_facts, "target_fit"),
        Some(("High".to_owned(), Some(workspace)))
    );
    assert_eq!(
        fact(&stodola_facts, "contact_quality"),
        Some(("Booking email".to_owned(), Some(workspace)))
    );
    assert_eq!(
        fact(&stodola_facts, "notes"),
        Some(("Three stages".to_owned(), Some(workspace)))
    );

    // The fact's clock is the sheet's Research_Date, at midnight UTC.
    let observed = sqlx::query_scalar::<_, time::OffsetDateTime>(
        "SELECT observed_at FROM place_venue_facts \
         WHERE venue_id = $1 AND attribute = 'capacity'",
    )
    .bind(stodola)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        observed,
        Date::from_calendar_date(2026, Month::September, 20)
            .expect("a valid date")
            .midnight()
            .assume_utc()
    );

    // ── Venue B: "searched, found nothing" is the tenant's record of its
    // own search, and a closed room is marked rather than dropped.
    let klub_b = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM place_venues \
         WHERE name_key = place_venue_key('Klub B') \
           AND city_id = (SELECT id FROM cities WHERE slug = 'poznan')",
    )
    .fetch_one(pool)
    .await?;
    let b_facts = facts(pool, klub_b).await?;
    assert_eq!(
        fact(&b_facts, "public_terms"),
        Some(("searched: none found".to_owned(), Some(workspace)))
    );
    assert_eq!(fact(&b_facts, "status"), Some(("closed".to_owned(), None)));

    // ── The unknown city produced no room and no facts — counted, not
    // guessed.
    let ghost = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM place_venues WHERE name_key = place_venue_key('Klub C')",
    )
    .fetch_optional(pool)
    .await?;
    assert_eq!(ghost, None, "an unresolvable city still filed a room");

    // ── A re-scan refreshes the claim rather than stacking a twin. The
    // same source_url is the same fact's key — only the value moves.
    let rescan = venue(
        "Stodoła",
        "Wrocław",
        |room| {
            room.capacity = Some(1600);
        },
        |_| {},
    );
    let rescan_report = SeedSheetReport {
        venues: vec![rescan],
        refusals: Vec::new(),
    };
    let resummary = repo.import_sheet(workspace, &rescan_report).await?;
    assert_eq!(resummary.imported, 1);
    let capacity_rows = sqlx::query(
        "SELECT value FROM place_venue_facts \
         WHERE venue_id = $1 AND attribute = 'capacity'",
    )
    .bind(stodola)
    .fetch_all(pool)
    .await?;
    assert_eq!(
        capacity_rows.len(),
        1,
        "re-import duplicated the capacity fact"
    );
    assert_eq!(capacity_rows[0].get::<String, _>("value"), "1600");

    Ok(())
}
