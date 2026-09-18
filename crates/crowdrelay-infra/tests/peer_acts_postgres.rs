//! Bill acts resolve to a tenant or a peer (§12-5, 4V.6).
//!
//! `event_acts.act_name` stopped being a bare string: every name written to a
//! bill now resolves inside the same transaction — to a tenant workspace when
//! the slug or a band listing's act name agrees on exactly one, or to a peer
//! act, the global band identity every tenant's bills point at. What these
//! tests pin down is the resolution contract itself: a listing name resolves,
//! an unknown name mints one peer row that a re-write re-links rather than
//! duplicating, and a slug/name ambiguity resolves nothing on the workspace
//! side — the peer link still lands, because the band exists even when the
//! tenant answer does not.

use std::time::Duration;

use crowdrelay_application::{EventActEntry, EventRepository, ReplaceEventActsCommand};
use crowdrelay_domain::{WorkspaceId, WorkspaceSlug};
use crowdrelay_infra::{config::DatabaseConfig, events::PostgresEventRepository};
use sqlx::{Connection, PgConnection, PgPool, postgres::PgPoolOptions};
use time::OffsetDateTime;
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
        let name = format!("crowdrelay_peeracts_{}", Uuid::now_v7().simple());
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

async fn seed_workspace(
    pool: &PgPool,
    id: Uuid,
    slug: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(slug)
        .bind("Peer Acts Test")
        .execute(pool)
        .await?;
    Ok(id)
}

/// The band's own listing — the act-name half of tenant resolution.
async fn seed_listing(
    pool: &PgPool,
    workspace_id: Uuid,
    act_name: &str,
    genre_tags: &[&str],
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO viryaos_band_listings (workspace_id, act_name, genre_tags) \
         VALUES ($1, $2, $3)",
    )
    .bind(workspace_id)
    .bind(act_name)
    .bind(genre_tags)
    .execute(pool)
    .await?;
    Ok(())
}

/// A published show in the writer's workspace — draft or published accepts a
/// bill, and the venue mark the write leaves behind is the same one the
/// comparable-acts read later joins on.
async fn seed_event(
    pool: &PgPool,
    workspace_id: Uuid,
    event_slug: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let city_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO cities (id, slug, name, country_code) \
         VALUES ($1, 'wroclaw', 'Wrocław', 'PL') \
         ON CONFLICT (country_code, slug) DO UPDATE SET name = EXCLUDED.name",
    )
    .bind(city_id)
    .execute(pool)
    .await?;
    let city_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM cities WHERE country_code = 'PL' AND slug = 'wroclaw'",
    )
    .fetch_one(pool)
    .await?;
    sqlx::query("INSERT INTO city_aggregates (workspace_id, city_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(workspace_id)
        .bind(city_id)
        .execute(pool)
        .await?;
    let event_id = Uuid::now_v7();
    let starts_at = OffsetDateTime::now_utc() + time::Duration::days(14);
    sqlx::query(
        r#"
        INSERT INTO events (
            id, workspace_id, city_id, slug, title, venue, starts_at,
            status, published_at
        ) VALUES ($1, $2, $3, $4, 'The night', 'Test Club', $5, 'published', now())
        "#,
    )
    .bind(event_id)
    .bind(workspace_id)
    .bind(city_id)
    .bind(event_slug)
    .bind(starts_at)
    .execute(pool)
    .await?;
    Ok(event_id)
}

/// The (workspace link, peer link) pair a bill row resolved to.
async fn act_links(
    pool: &PgPool,
    workspace_id: Uuid,
    event_id: Uuid,
    act_slug: &str,
) -> Result<(Option<Uuid>, Option<Uuid>), sqlx::Error> {
    sqlx::query_as::<_, (Option<Uuid>, Option<Uuid>)>(
        "SELECT act_workspace_id, peer_act_id FROM event_acts \
         WHERE workspace_id = $1 AND event_id = $2 AND act_slug = $3",
    )
    .bind(workspace_id)
    .bind(event_id)
    .bind(act_slug)
    .fetch_one(pool)
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_bill_write_resolves_acts_to_tenants_and_peers() -> Result<(), Box<dyn std::error::Error>>
{
    let database = DisposableDatabase::create().await?;
    let result = run_cases(&database.pool).await;
    database.drop_database().await;
    result
}

async fn run_cases(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    // The writer's own workspace — the one the repository is scoped to.
    let writer_id = WorkspaceId::new();
    let writer_slug =
        WorkspaceSlug::parse(format!("peer-writer-{}", writer_id.into_uuid().simple()))?;
    seed_workspace(pool, writer_id.into_uuid(), writer_slug.as_str()).await?;
    let event_id = seed_event(pool, writer_id.into_uuid(), "the-night-2026").await?;

    // Two other tenants: bravo carries a band listing, charlie only exists as
    // a slug — the pair the ambiguity case needs.
    let bravo_id = seed_workspace(pool, Uuid::now_v7(), "tenant-bravo").await?;
    seed_listing(pool, bravo_id, "The Neighbour Band", &["metal"]).await?;
    let charlie_id = seed_workspace(pool, Uuid::now_v7(), "tenant-charlie").await?;

    let database = DatabaseConfig {
        url: "postgres://unused/disposable".to_owned(),
        max_connections: 2,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
    };
    let events = PostgresEventRepository::new(pool.clone(), writer_slug, &database, Vec::new());

    let act = |slug: &str, name: &str| EventActEntry {
        act_slug: slug.to_owned(),
        act_name: name.to_owned(),
        position: 0,
        ticket_url: None,
    };
    let bill = vec![
        // Listing-name hit on bravo — the slug matches nobody.
        act("support-slot", "The Neighbour Band"),
        // Slug hit on bravo — the name matches no listing.
        act("tenant-bravo", "Headliner"),
        // Neither — an unknown band becomes a peer.
        act("mystery-act", "The Mystery Act"),
        // Slug hits charlie while the name hits bravo's listing: ambiguous.
        act("tenant-charlie", "The Neighbour Band"),
    ];
    events
        .replace_event_acts(&ReplaceEventActsCommand {
            workspace_id: writer_id,
            event_slug: "the-night-2026".to_owned(),
            acts: bill.clone(),
        })
        .await?;

    // ── A listing's act name resolves to the tenant that owns it. ────────
    let (ws_link, peer_link) =
        act_links(pool, writer_id.into_uuid(), event_id, "support-slot").await?;
    assert_eq!(ws_link, Some(bravo_id));
    assert_eq!(peer_link, None, "a resolved tenant mints no peer");

    // ── A bare slug hit resolves the same way. ───────────────────────────
    let (ws_link, peer_link) =
        act_links(pool, writer_id.into_uuid(), event_id, "tenant-bravo").await?;
    assert_eq!(ws_link, Some(bravo_id));
    assert_eq!(peer_link, None);

    // ── An unknown act mints its peer row and links it. ──────────────────
    let (ws_link, peer_link) =
        act_links(pool, writer_id.into_uuid(), event_id, "mystery-act").await?;
    assert_eq!(ws_link, None);
    let mystery_peer = peer_link.ok_or("an unresolved act must link a peer")?;
    let (name_key, display_name) = sqlx::query_as::<_, (String, String)>(
        "SELECT name_key, display_name FROM place_peer_acts WHERE id = $1",
    )
    .bind(mystery_peer)
    .fetch_one(pool)
    .await?;
    assert_eq!(name_key, "the mystery act");
    assert_eq!(display_name, "The Mystery Act");

    // ── Ambiguity resolves nothing on the workspace side — and the peer
    // link still lands, because the band exists even when the tenant
    // answer does not. ──────────────────────────────────────────────────
    let (ws_link, peer_link) =
        act_links(pool, writer_id.into_uuid(), event_id, "tenant-charlie").await?;
    assert_eq!(
        ws_link, None,
        "a slug hit and a listing hit for different workspaces resolve nothing"
    );
    let ambiguous_peer = peer_link.ok_or("an ambiguous act still gets a peer")?;
    let neighbour_key =
        sqlx::query_scalar::<_, String>("SELECT name_key FROM place_peer_acts WHERE id = $1")
            .bind(ambiguous_peer)
            .fetch_one(pool)
            .await?;
    assert_eq!(neighbour_key, "the neighbour band");

    // Two peers minted — the resolved tenant acts produced none.
    let peer_count = sqlx::query_scalar::<_, i64>("SELECT count(*)::bigint FROM place_peer_acts")
        .fetch_one(pool)
        .await?;
    assert_eq!(peer_count, 2);

    // ── Re-writing the same bill re-resolves the same peers — no dupes. ──
    let mut rewritten = bill;
    rewritten[2] = act("mystery-act", "THE MYSTERY ACT");
    events
        .replace_event_acts(&ReplaceEventActsCommand {
            workspace_id: writer_id,
            event_slug: "the-night-2026".to_owned(),
            acts: rewritten,
        })
        .await?;

    let (ws_link, peer_link) =
        act_links(pool, writer_id.into_uuid(), event_id, "mystery-act").await?;
    assert_eq!(ws_link, None);
    assert_eq!(
        peer_link,
        Some(mystery_peer),
        "a re-written bill re-links the same peer rather than minting a twin"
    );
    let peer_count = sqlx::query_scalar::<_, i64>("SELECT count(*)::bigint FROM place_peer_acts")
        .fetch_one(pool)
        .await?;
    assert_eq!(peer_count, 2, "the rewrite minted duplicates");

    // ON CONFLICT DO UPDATE — the newest spelling wins the display name.
    let display_name =
        sqlx::query_scalar::<_, String>("SELECT display_name FROM place_peer_acts WHERE id = $1")
            .bind(mystery_peer)
            .fetch_one(pool)
            .await?;
    assert_eq!(display_name, "THE MYSTERY ACT");

    // The ambiguous act still resolves to no tenant, and never to charlie.
    let (ws_link, _) = act_links(pool, writer_id.into_uuid(), event_id, "tenant-charlie").await?;
    assert_ne!(ws_link, Some(charlie_id));
    assert_eq!(ws_link, None);

    Ok(())
}

/// The 0316 seed resolves spelling families the way the map was curated
/// (N.3): alias spellings land on the family canonical, deliberately
/// distinct neighbours stay distinct, and canonicals are self-mapped so the
/// set is inspectable without knowing which rows were omitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn genre_alias_seed_resolves_spellings() -> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = async {
        let pool = &database.pool;
        let resolved = |alias: &'static str| {
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT canonical FROM place_genre_aliases WHERE alias = $1",
            )
            .bind(alias)
            .fetch_one(pool)
        };
        // Spelling families fold to their family canonical — including the
        // whitespace/punctuation variants place_venue_key leaves literal.
        for (alias, canonical) in [
            ("doom", "doom metal"),
            ("doommetal", "doom metal"),
            ("metal core", "metalcore"),
            ("melodic metalcore", "metalcore"),
            ("djent metal", "djent"),
            ("post-metal", "post-metal"),
            ("post metal", "post-metal"),
            ("postmetal", "post-metal"),
            ("nu-metal", "nu metal"),
            ("thrash", "thrash metal"),
            ("prog metal", "progressive metal"),
        ] {
            assert_eq!(
                resolved(alias).await?,
                Some(canonical.to_owned()),
                "alias {alias} must resolve to {canonical}"
            );
        }
        // Deliberately distinct neighbours stay distinct — broadening a real
        // boundary to buy a match is how this map would lie.
        for (alias, canonical) in [
            ("jungle", "jungle"),
            ("drum and bass", "drum and bass"),
            ("melodic hardcore", "melodic hardcore"),
            ("hardcore", "hardcore"),
            ("screamo", "screamo"),
        ] {
            assert_eq!(
                resolved(alias).await?,
                Some(canonical.to_owned()),
                "{alias} must not fold into a neighbour's canonical"
            );
        }
        // Self-mapped canonicals document the resolve target spellings.
        for canonical in [
            "metalcore",
            "deathcore",
            "doom metal",
            "black metal",
            "progressive metal",
        ] {
            assert_eq!(
                resolved(canonical).await?,
                Some(canonical.to_owned()),
                "canonical {canonical} must be self-mapped"
            );
        }
        Ok(())
    }
    .await;
    database.drop_database().await;
    result
}
