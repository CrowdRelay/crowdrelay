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

mod common;

use std::time::Duration;

use crowdrelay_application::{EventActEntry, EventRepository, ReplaceEventActsCommand};
use crowdrelay_domain::{WorkspaceId, WorkspaceSlug};
use crowdrelay_infra::{config::DatabaseConfig, events::PostgresEventRepository};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

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
        "INSERT INTO band_listings (workspace_id, act_name, genre_tags) \
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
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run_cases(&database).await
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

    // First casing wins, matching place_venues' policy — a later bill
    // rewrite does not get to re-spell the band.
    let display_name =
        sqlx::query_scalar::<_, String>("SELECT display_name FROM place_peer_acts WHERE id = $1")
            .bind(mystery_peer)
            .fetch_one(pool)
            .await?;
    assert_eq!(display_name, "The Mystery Act");

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
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    async {
        let pool = &database;
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
    .await
}

/// §N.4 — the package matcher: a consented roster sibling is named on the
/// bill, priced by the part of its audience that is genuinely new in that
/// city. Union minus intersection, per city, against real rows — every number
/// the proposal quotes is a join that could silently measure the wrong thing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_consented_sibling_is_named_on_the_bill_with_its_arithmetic()
-> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run_package(&database).await
}

async fn run_package(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_domain::gig_plan::{Reason, TenantIntent, plan_gig};
    use crowdrelay_infra::gig_planning::city_opportunities;

    let org = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO organizations (slug, name) VALUES ('label-n4', 'Label N4') RETURNING id",
    )
    .fetch_one(pool)
    .await?;
    let member = |name: &str| {
        let pool = pool.clone();
        let name = name.to_owned();
        async move {
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO workspaces (id, slug, name, organization_id)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(id)
            .bind(format!("ws-{name}"))
            .bind(&name)
            .bind(org)
            .execute(&pool)
            .await?;
            Ok::<Uuid, Box<dyn std::error::Error>>(id)
        }
    };
    let head = member("headliner").await?;
    let support = member("support").await?;
    let third = member("third-act").await?;
    let _quiet = member("quiet-act").await?;

    let city = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ('wroclaw-n4', 'wroclaw-n4', 'PL', 51.1, 17.0) RETURNING id",
    )
    .fetch_one(pool)
    .await?;
    let reachable = |ws: Uuid, email: &str| {
        let pool = pool.clone();
        let email = email.to_owned();
        async move {
            let fan = sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO fans (workspace_id, normalized_email, status)
                 VALUES ($1, $2, 'active') RETURNING id",
            )
            .bind(ws)
            .bind(&email)
            .fetch_one(&pool)
            .await?;
            sqlx::query(
                "INSERT INTO fan_consents
                    (workspace_id, fan_id, purpose, granted, policy_version, source, recorded_at)
                 VALUES ($1, $2, 'marketing', true, 'v1', 'signup', now())",
            )
            .bind(ws)
            .bind(fan)
            .execute(&pool)
            .await?;
            sqlx::query(
                "INSERT INTO fan_location_preferences
                    (workspace_id, fan_id, city_id, radius_km, nearby_gigs_enabled)
                 VALUES ($1, $2, $3, 50, true)",
            )
            .bind(ws)
            .bind(fan)
            .bind(city)
            .execute(&pool)
            .await?;
            sqlx::query(
                "INSERT INTO fan_city_interests (workspace_id, fan_id, city_id)
                 VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            )
            .bind(ws)
            .bind(fan)
            .bind(city)
            .execute(&pool)
            .await?;
            Ok::<(), Box<dyn std::error::Error>>(())
        }
    };
    // The headliner reaches sixty — over the planner's floor. The support
    // reaches four, two of them the same people; a third sibling reaches its
    // own three but never consented; a fourth reaches nobody here at all.
    for index in 0..60 {
        reachable(head, &format!("head{index}@example.com")).await?;
    }
    for index in 0..2 {
        reachable(support, &format!("head{index}@example.com")).await?;
        reachable(support, &format!("own{index}@example.com")).await?;
    }
    for index in 0..3 {
        reachable(third, &format!("third{index}@example.com")).await?;
    }
    // A room the headliner has played, and somebody to write to.
    sqlx::query(
        "INSERT INTO events (workspace_id, city_id, slug, title, venue, starts_at, status)
         VALUES ($1, $2, 'n4-show', 'n4-show', 'Klub N4', now() - interval '40 days', 'completed')",
    )
    .bind(head)
    .bind(city)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO booking_targets
            (workspace_id, city_id, target_kind, display_name, contact_email,
             relationship_score, capacity)
         VALUES ($1, $2, 'promoter', 'Anna', 'anna@example.com', 70, 300)",
    )
    .bind(head)
    .bind(city)
    .execute(pool)
    .await?;
    // The consent that makes the support askable: an active event_crossbill
    // *offered by the support* — `from` is the audience owner who acted. A
    // consent in the other direction proves only that the headliner
    // consented, which is exactly what `third-act` carries: an active edge
    // from head to it, plus a revoked one in the agreeing direction. Neither
    // makes it askable — the bill may measure it but never name it.
    sqlx::query(
        "INSERT INTO amplification_consents
            (organization_id, from_workspace_id, to_workspace_id, purpose,
             status, approved_by, approved_at)
         VALUES ($1, $2, $3, 'event_crossbill', 'active', 'support', now())",
    )
    .bind(org)
    .bind(support)
    .bind(head)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO amplification_consents
            (organization_id, from_workspace_id, to_workspace_id, purpose,
             status, approved_by, approved_at)
         VALUES ($1, $2, $3, 'event_crossbill', 'active', 'label', now())",
    )
    .bind(org)
    .bind(head)
    .bind(third)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO amplification_consents
            (organization_id, from_workspace_id, to_workspace_id, purpose,
             status, approved_by, approved_at, revoked_at, revoke_reason)
         VALUES ($1, $2, $3, 'event_crossbill', 'revoked', 'label', now(),
                 now(), 'asked to be left off bills')",
    )
    .bind(org)
    .bind(third)
    .bind(head)
    .execute(pool)
    .await?;

    let opportunities = city_opportunities(pool, head, OffsetDateTime::now_utc()).await?;
    let wro = opportunities
        .iter()
        .find(|city| city.city == "wroclaw-n4")
        .ok_or("the city the headliner can play was not considered")?;

    let support_entry = wro
        .co_bill
        .iter()
        .find(|act| act.name == "support")
        .ok_or("a consented sibling with reach was not a co-bill candidate")?;
    assert_eq!(support_entry.reachable_here, 4);
    assert_eq!(
        support_entry.audience_overlap_basis_points, 5_000,
        "two of the support's four are the headliner's people too"
    );
    assert!(support_entry.consented_to_share_bills);

    let third_entry = wro
        .co_bill
        .iter()
        .find(|act| act.name == "third-act")
        .ok_or("a sibling with reach and no consent was dropped instead of flagged")?;
    assert!(
        !third_entry.consented_to_share_bills,
        "an active edge the sibling never offered, and a revoked one it did,          still must not make it askable"
    );
    assert!(
        wro.co_bill.iter().all(|act| act.name != "quiet-act"),
        "a sibling reaching nobody here is absent — unmeasured, not a zero"
    );

    // The proposal: acts named, city named, the room's size on the evidence,
    // and the arithmetic — four reached, two already ours, two added.
    let plan = plan_gig(wro, TenantIntent::BookingShows).expect("a route, a room and a bill");
    assert_eq!(plan.city, "wroclaw-n4");
    assert_eq!(plan.invite_to_bill, ["support".to_owned()]);
    assert_eq!(
        plan.reach.added_by_co_bill, 2,
        "union minus intersection: four reached, two already ours"
    );
    assert!(
        plan.reasons.iter().any(|reason| matches!(
            reason,
            Reason::CoBillAddsAudience { act, adds_reachable }
                if act == "support" && *adds_reachable == 2
        )),
        "the arithmetic behind the bill never reached the proposal"
    );
    Ok(())
}
