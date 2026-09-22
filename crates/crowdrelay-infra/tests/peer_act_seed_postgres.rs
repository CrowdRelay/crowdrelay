//! The peer-act seed intake: a researched band sheet's rows must land in the
//! shared registry with every claim attributed — the act's public facts
//! global, the contributing tenant's contact scoped to the workspace that
//! wrote it.
//!
//! Migration 0325 is the first writer `place_peer_act_facts` has and the
//! first carrier of `place_peer_acts.home_city_id`, so the only way to know
//! the split holds is to drive the table. Each case below is a way the
//! import was wrong at some point: a contact address leaking into the
//! shared half, a re-scan erasing a home city a previous sheet knew, an
//! unresolvable city being guessed rather than left NULL, or the same act
//! named by two sheets minting two identities.

use crowdrelay_domain::peer_act_seed::{
    PeerActSeedReport, SeededPeer, SeededPeerAct, SeededPeerActView,
};
use crowdrelay_infra::peer_act_seed::PostgresPeerActSeedRepository;
use sqlx::{Connection, PgConnection, PgPool, Row, postgres::PgPoolOptions};
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
        let name = format!("crowdrelay_peerseed_{}", Uuid::now_v7().simple());
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

/// A row the way `domain::peer_act_seed::parse_seed_row` would produce it.
fn act(
    name: &str,
    fill: impl FnOnce(&mut SeededPeerAct),
    view: impl FnOnce(&mut SeededPeerActView),
) -> SeededPeer {
    let mut seeded = SeededPeerAct {
        name: name.to_owned(),
        country: None,
        city: None,
        genre_tags: Vec::new(),
        social: Some(format!("https://facebook.com/{}", name.replace(' ', ""))),
        website: None,
        source_url: Some(format!("https://source.example/{name}")),
        activity: Some("2026 activity verified".to_owned()),
        researched_on: Some("2026-09-19".to_owned()),
    };
    fill(&mut seeded);
    let mut seeded_view = SeededPeerActView::default();
    view(&mut seeded_view);
    SeededPeer {
        act: seeded,
        view: seeded_view,
    }
}

/// The facts an act carries, as (attribute, value, workspace_id) — the
/// workspace being NULL is the whole assertion: a NULL here is the global
/// half, the workspace's own id is the private half.
async fn facts(
    pool: &PgPool,
    peer_act_id: Uuid,
) -> Result<Vec<(String, String, Option<Uuid>)>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT attribute, value, workspace_id FROM place_peer_act_facts \
         WHERE peer_act_id = $1 ORDER BY attribute, value",
    )
    .bind(peer_act_id)
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
async fn a_band_sheet_lands_as_attributed_peer_facts() -> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = run_cases(&database.pool).await;
    database.drop_database().await;
    result
}

async fn run_cases(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = seed_workspace(pool).await?;
    let repo = PostgresPeerActSeedRepository::new(pool.clone());

    let report = PeerActSeedReport {
        acts: vec![
            // The full row: a resolvable city, genres, a contact the tenant
            // found, and the band's own pages. "Wrocław" exercises the
            // name-match path — the catalogue slug is ASCII, the sheet's
            // spelling is not.
            act(
                "Hortus",
                |act| {
                    act.city = Some("Wrocław".to_owned());
                    act.genre_tags = vec!["hard rock".to_owned(), "heavy metal".to_owned()];
                    act.website = Some("https://hortus.example.com".to_owned());
                },
                |view| {
                    view.email = Some("hortus@example.com".to_owned());
                    view.notes = Some("played Bolko twice".to_owned());
                },
            ),
            // The minimal row the real seed carries — a name and a social
            // page, no city. The act is still worth the registry: it feeds
            // the graph by genre later, and a NULL city is an honest
            // "nobody said", not a guess.
            act("30zeta", |_| {}, |_| {}),
            // A city the catalogue does not know: the act imports with
            // `home_city_id` NULL and the summary counts the gap rather than
            // filing the band against a place it may not be.
            act(
                "Nowhere Band",
                |act| {
                    act.city = Some("Notarealcity".to_owned());
                },
                |_| {},
            ),
        ],
        refusals: Vec::new(),
    };
    let summary = repo.import_sheet(workspace, &report).await?;
    assert_eq!(summary.imported, 3);
    assert_eq!(summary.unresolved_city, 1);

    let hortus =
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM place_peer_acts WHERE name_key = 'hortus'")
            .fetch_one(pool)
            .await?;

    // The resolved pointer and the global fact say the same thing.
    let home_city = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT home_city_id FROM place_peer_acts WHERE id = $1",
    )
    .bind(hortus)
    .fetch_one(pool)
    .await?;
    let wroclaw = sqlx::query_scalar::<_, Uuid>("SELECT id FROM cities WHERE slug = 'wroclaw'")
        .fetch_one(pool)
        .await?;
    assert_eq!(home_city, Some(wroclaw));

    let hortus_facts = facts(pool, hortus).await?;
    // Contact and notes are the tenant's lead — never global.
    assert!(hortus_facts.contains(&(
        "contact_email".to_owned(),
        "hortus@example.com".to_owned(),
        Some(workspace),
    )));
    assert!(hortus_facts.contains(&(
        "notes".to_owned(),
        "played Bolko twice".to_owned(),
        Some(workspace),
    )));
    // Public pages, the home-city claim (the sheet's own spelling, not the
    // resolved id) and the activity finding are the world's knowledge.
    for (attribute, value) in [
        ("link:social", "https://facebook.com/Hortus"),
        ("link:website", "https://hortus.example.com"),
        ("home_city", "Wrocław"),
        ("activity", "2026 activity verified"),
    ] {
        assert!(
            hortus_facts.contains(&(attribute.to_owned(), value.to_owned(), None)),
            "missing global fact {attribute}={value}: {hortus_facts:?}"
        );
    }
    // Nothing private leaked into the global half.
    assert!(
        hortus_facts
            .iter()
            .all(|(attribute, _, scope)| scope.is_some()
                || !attribute.starts_with("contact") && attribute != "notes"),
        "a private attribute landed global: {hortus_facts:?}"
    );

    // Genres landed in the typed table the peer graph reads, attributed.
    let genres: Vec<String> = sqlx::query_scalar(
        "SELECT genre_tag FROM place_peer_act_genres WHERE peer_act_id = $1 ORDER BY genre_tag",
    )
    .bind(hortus)
    .fetch_all(pool)
    .await?;
    assert_eq!(genres, vec!["hard rock", "heavy metal"]);

    // The minimal row imported with NULLs, not guesses.
    let zeta =
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM place_peer_acts WHERE name_key = '30zeta'")
            .fetch_one(pool)
            .await?;
    let zeta_city = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT home_city_id FROM place_peer_acts WHERE id = $1",
    )
    .bind(zeta)
    .fetch_one(pool)
    .await?;
    assert_eq!(zeta_city, None);
    let zeta_facts = facts(pool, zeta).await?;
    assert_eq!(
        zeta_facts,
        vec![
            (
                "activity".to_owned(),
                "2026 activity verified".to_owned(),
                None
            ),
            (
                "link:social".to_owned(),
                "https://facebook.com/30zeta".to_owned(),
                None
            ),
        ]
    );

    // The unresolvable city left NULL, not a guess.
    let nowhere = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT home_city_id FROM place_peer_acts WHERE name_key = 'nowhere band'",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(nowhere, None);

    // A second sheet naming the same act a different way: one identity, a
    // refreshed claim, no twin. And a sheet without a City cell must not
    // erase the home city an earlier sheet resolved.
    let rescan = PeerActSeedReport {
        acts: vec![act(
            "HORTUS ",
            |act| {
                act.city = None;
                act.genre_tags = vec!["hard rock".to_owned()];
                act.social = Some("https://facebook.com/Hortus".to_owned());
                act.source_url = Some("https://source.example/HORTUS".to_owned());
            },
            |_| {},
        )],
        refusals: Vec::new(),
    };
    let resummary = repo.import_sheet(workspace, &rescan).await?;
    assert_eq!(resummary.imported, 1);
    let act_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM place_peer_acts WHERE name_key = 'hortus'",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(act_count, 1, "a re-scan minted a second identity");
    let still_home = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT home_city_id FROM place_peer_acts WHERE id = $1",
    )
    .bind(hortus)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        still_home,
        Some(wroclaw),
        "a cityless re-scan erased the known home city"
    );

    // A second workspace's sheet holds its own lead — the private half keys
    // on the contributing workspace, never overwriting another's.
    let other_workspace = seed_workspace(pool).await?;
    let other_sheet = PeerActSeedReport {
        acts: vec![act(
            "Hortus",
            |_| {},
            |view| {
                view.email = Some("booking@hortus-crew.example".to_owned());
            },
        )],
        refusals: Vec::new(),
    };
    repo.import_sheet(other_workspace, &other_sheet).await?;
    let hortus_facts = facts(pool, hortus).await?;
    assert!(hortus_facts.contains(&(
        "contact_email".to_owned(),
        "hortus@example.com".to_owned(),
        Some(workspace),
    )));
    assert!(hortus_facts.contains(&(
        "contact_email".to_owned(),
        "booking@hortus-crew.example".to_owned(),
        Some(other_workspace),
    )));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_shared_slug_resolves_to_the_sheet_rows_own_country()
-> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = run_country_scoped_case(&database.pool).await;
    database.drop_database().await;
    result
}

/// `cities.slug` is unique per country, not globally — a "Neustadt" cell in
/// a Czechia row must land on the Czech city even when Germany's catalogue
/// carries the same slug, and the name-match must never pick one by
/// accident.
async fn run_country_scoped_case(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = seed_workspace(pool).await?;
    let repo = PostgresPeerActSeedRepository::new(pool.clone());

    let de_neustadt = Uuid::now_v7();
    let cz_neustadt = Uuid::now_v7();
    for (id, country) in [(de_neustadt, "DE"), (cz_neustadt, "CZ")] {
        sqlx::query("INSERT INTO cities (id, slug, name, country_code) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind("neustadt")
            .bind("Neustadt")
            .bind(country)
            .execute(pool)
            .await?;
    }

    let report = PeerActSeedReport {
        acts: vec![
            act(
                "Czech Act",
                |act| {
                    act.city = Some("Neustadt".to_owned());
                    act.country = Some("Czechia".to_owned());
                },
                |_| {},
            ),
            // Same city cell, no country the resolver knows: two cities
            // answer, so the honest outcome is no pointer at all.
            act(
                "Ambiguous Act",
                |act| act.city = Some("Neustadt".to_owned()),
                |_| {},
            ),
        ],
        refusals: Vec::new(),
    };
    let summary = repo.import_sheet(workspace, &report).await?;
    assert_eq!(summary.imported, 2);
    assert_eq!(summary.unresolved_city, 1);

    let resolved = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT home_city_id FROM place_peer_acts WHERE name_key = 'czech act'",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(
        resolved,
        Some(cz_neustadt),
        "the sheet's own country did not scope the city resolution"
    );
    let ambiguous = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT home_city_id FROM place_peer_acts WHERE name_key = 'ambiguous act'",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(ambiguous, None, "an ambiguous name picked a city at random");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn one_bad_row_does_not_abort_the_sheet() -> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = run_failure_isolation_case(&database.pool).await;
    database.drop_database().await;
    result
}

/// A row whose own transaction fails is counted, not propagated: the
/// sheet's other bands still land. The failing row here is a
/// whitespace-only name — `place_venue_key` normalizes it to NULL and the
/// NOT NULL constraint refuses it — and a 600-char name exercises the
/// name_key bound: it must import on a digested key rather than violate the
/// 500-char CHECK or silently merge with another long name.
async fn run_failure_isolation_case(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = seed_workspace(pool).await?;
    let repo = PostgresPeerActSeedRepository::new(pool.clone());

    let long_name = format!("{}{}", "The ".repeat(150), "Band"); // 604 chars
    let report = PeerActSeedReport {
        acts: vec![
            act("Fine Act", |_| {}, |_| {}),
            act("   ", |_| {}, |_| {}),
            act(&long_name, |_| {}, |_| {}),
        ],
        refusals: Vec::new(),
    };
    let summary = repo.import_sheet(workspace, &report).await?;
    assert_eq!(summary.imported, 2, "a bad row took the sheet down with it");
    assert_eq!(summary.failed, 1);

    let key = sqlx::query_scalar::<_, String>(
        "SELECT name_key FROM place_peer_acts WHERE display_name = $1",
    )
    .bind(&long_name[..500])
    .fetch_one(pool)
    .await?;
    assert!(
        key.len() <= 500,
        "name_key {} chars violates the CHECK bound",
        key.len()
    );
    // The digested key is deterministic: a re-scan of the same over-long
    // name upserts the same identity rather than minting a twin.
    let rescan = PeerActSeedReport {
        acts: vec![act(&long_name, |_| {}, |_| {})],
        refusals: Vec::new(),
    };
    repo.import_sheet(workspace, &rescan).await?;
    let ids = sqlx::query_scalar::<_, Uuid>("SELECT id FROM place_peer_acts WHERE name_key = $1")
        .bind(&key)
        .fetch_all(pool)
        .await?;
    assert_eq!(ids.len(), 1);
    Ok(())
}
