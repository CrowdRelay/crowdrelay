//! The Rust and SQL canonicalisers must agree, case for case.
//!
//! `canonical_place_url` in `crowdrelay_domain::audience_graph` decides what the
//! application writes. `crowdrelay_canonical_place_url` in migration 0259 decides
//! what the CHECK constraint accepts and what the merge collapsed. Two definitions
//! of "same place" is the shape of every drift bug in this codebase: if the SQL
//! side folds something the Rust side does not, the application writes a URL its
//! own constraint rejects and every discovery insert fails; if the Rust side folds
//! something the SQL side does not, duplicates come back and with them a second
//! post to the same community under the band's name.
//!
//! So this compares them directly rather than testing either alone.

use crowdrelay_domain::audience_graph::canonical_place_url;
use sqlx::{Connection, PgConnection, PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

/// Every spelling worth pinning, including the ones production actually held.
const CASES: &[&str] = &[
    // The duplicates measured in production 2026-09-13.
    "https://www.reddit.com/r/MetalForTheMasses",
    "/r/MetalForTheMasses/",
    "https://reddit.com/r/Djent",
    "https://www.reddit.com/r/Djent",
    "https://www.reddit.com/r/MetalMemes",
    "https://www.reddit.com/r/metalmemes",
    "/r/ListenToThis/",
    "https://reddit.com/r/listentothis",
    // Host and scheme spellings.
    "http://reddit.com/r/metal",
    "https://old.reddit.com/r/metal",
    "https://new.reddit.com/r/metal",
    "r/metal",
    "/r/metal",
    "https://www.reddit.com/r/metal/",
    // Must NOT fold: a website sharing a subreddit's name.
    "https://inmetalwetrust.club",
    // Must NOT fold: a post inside a subreddit is not the subreddit.
    "https://www.reddit.com/r/Metal/comments/abc123/some_title/",
    // Must NOT fold: names Reddit could not have issued.
    "https://www.reddit.com/r/",
    "https://www.reddit.com/r/has-a-hyphen",
    "https://www.reddit.com/r/way_too_long_to_be_a_real_subreddit",
    "https://www.reddit.com/user/someone",
    // Other platforms.
    "https://discord.gg/abc123",
    "https://www.facebook.com/groups/12345",
    "https://open.spotify.com/playlist/xyz",
    // Whitespace is a spelling too.
    "  https://www.reddit.com/r/Djent  ",
    "  https://discord.gg/x  ",
];

struct DisposableDatabase {
    pool: PgPool,
    admin_url: String,
    name: String,
}

impl DisposableDatabase {
    async fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let base = std::env::var("CROWDRELAY_TEST_DATABASE_URL")
            .map_err(|_| "CROWDRELAY_TEST_DATABASE_URL must target a disposable database")?;
        let name = format!("crowdrelay_placeurl_{}", Uuid::now_v7().simple());
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_rust_and_sql_canonicalisers_agree() -> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = compare(&database.pool).await;
    database.drop_database().await;
    result
}

async fn compare(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    for case in CASES {
        let sql: String = sqlx::query_scalar("SELECT crowdrelay_canonical_place_url($1)")
            .bind(*case)
            .fetch_one(pool)
            .await
            .map_err(|error| format!("SQL canonicaliser failed for {case:?}: {error}"))?;
        let rust = canonical_place_url(case);
        if !(rust == sql) {
            return Err(
                format!("canonicalisers disagree for {case:?}: rust={rust:?} sql={sql:?}").into(),
            );
        }
    }

    // Idempotence, on both sides. The CHECK constraint asserts
    // `url = canonical(url)`, so a canonical form that is not a fixed point would
    // make every insert fail.
    for case in CASES {
        let once = canonical_place_url(case);
        if !(canonical_place_url(&once) == once) {
            return Err(format!("rust canonicaliser is not idempotent for {case:?}").into());
        }
        let sql_twice: String = sqlx::query_scalar(
            "SELECT crowdrelay_canonical_place_url(crowdrelay_canonical_place_url($1))",
        )
        .bind(*case)
        .fetch_one(pool)
        .await?;
        if !(sql_twice == once) {
            return Err(format!(
                "sql canonicaliser is not idempotent for {case:?}: {sql_twice:?} vs {once:?}"
            )
            .into());
        }
    }

    // And the constraint actually refuses a non-canonical row, so the net under
    // the application is real rather than declared.
    let workspace = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace)
        .bind(format!("placeurl-{}", workspace.simple()))
        .bind("Place URL test")
        .execute(pool)
        .await?;
    let refused = sqlx::query(
        "INSERT INTO discovery_places (workspace_id, place_kind, platform, name, url) \
         VALUES ($1, 'subreddit', 'reddit', 'Djent', 'https://reddit.com/r/Djent')",
    )
    .bind(workspace)
    .execute(pool)
    .await;
    if refused.is_ok() {
        return Err(
            "a non-canonical URL must be refused by discovery_places_url_is_canonical".into(),
        );
    }
    // The canonical form of the same place inserts fine.
    sqlx::query(
        "INSERT INTO discovery_places (workspace_id, place_kind, platform, name, url) \
         VALUES ($1, 'subreddit', 'reddit', 'Djent', 'https://www.reddit.com/r/djent')",
    )
    .bind(workspace)
    .execute(pool)
    .await
    .map_err(|error| format!("the canonical form must insert: {error}"))?;

    Ok(())
}
