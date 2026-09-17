//! The shared night, end to end (§12-9, Sprint 4V.6b).
//!
//! Two tenants' events at the same room on the same UTC date land on one
//! `place_events` row; what each side sees of that row is the lens its
//! relationship derives, and nothing else. These tests pin the boundary the
//! whole sprint exists for: the organiser's payload carries sums and never
//! a per-act terms field, the co-billed act sees the lineup and the public
//! halves but never `own_terms` or draw parts, and a band's confirmation of
//! itself cannot be forged by the tenant that billed it.

use crowdrelay_domain::night::ContributionKind;
use crowdrelay_infra::night::{NightError, PostgresNightRepository};
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
        let name = format!("crowdrelay_night_{}", Uuid::now_v7().simple());
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

async fn seed_workspace(pool: &PgPool, slug: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(slug)
        .bind("Night Test")
        .execute(pool)
        .await?;
    Ok(id)
}

/// One Wrocław room, shared by every event this test seeds.
async fn seed_city(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO cities (id, slug, name, country_code) \
         VALUES ($1, 'wroclaw', 'Wrocław', 'PL') \
         ON CONFLICT (country_code, slug) DO UPDATE SET name = EXCLUDED.name",
    )
    .bind(Uuid::now_v7())
    .execute(pool)
    .await?;
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM cities WHERE country_code = 'PL' AND slug = 'wroclaw'",
    )
    .fetch_one(pool)
    .await?)
}

/// A published show at the room — the venue registry's own trigger turns the
/// (venue, city, status) claim into the mark, and the rendezvous trigger
/// turns the mark into the night.
async fn seed_event(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    event_slug: &str,
    venue: &str,
    starts_at: OffsetDateTime,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    sqlx::query("INSERT INTO city_aggregates (workspace_id, city_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(workspace_id)
        .bind(city_id)
        .execute(pool)
        .await?;
    let event_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO events (
            id, workspace_id, city_id, slug, title, venue, starts_at,
            status, published_at
        ) VALUES ($1, $2, $3, $4, 'The night', $5, $6, 'published', now())
        "#,
    )
    .bind(event_id)
    .bind(workspace_id)
    .bind(city_id)
    .bind(event_slug)
    .bind(venue)
    .bind(starts_at)
    .execute(pool)
    .await?;
    Ok(event_id)
}

/// A bill row claiming `act_workspace_id`'s act — written directly, because
/// the resolution path that would set it has its own suite (0310); what is
/// under test here is the boundary the claim creates.
async fn bill_act(
    pool: &PgPool,
    bill_workspace: Uuid,
    event_id: Uuid,
    act_workspace: Option<Uuid>,
    act_slug: &str,
    act_name: &str,
    position: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO event_acts (workspace_id, event_id, act_slug, act_name, position, act_workspace_id) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(bill_workspace)
    .bind(event_id)
    .bind(act_slug)
    .bind(act_name)
    .bind(position)
    .bind(act_workspace)
    .execute(pool)
    .await?;
    Ok(())
}

async fn place_event_of(pool: &PgPool, event_id: Uuid) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<Uuid>>("SELECT place_event_id FROM events WHERE id = $1")
        .bind(event_id)
        .fetch_one(pool)
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn two_workspaces_share_one_night() -> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = async {
        let pool = &database.pool;
        let city_id = seed_city(pool).await?;
        let alpha = seed_workspace(pool, "night-alpha").await?;
        let bravo = seed_workspace(pool, "night-bravo").await?;
        // Same room, same UTC date, two tenants — one night.
        let starts = OffsetDateTime::now_utc() + time::Duration::days(14);
        let event_a = seed_event(pool, alpha, city_id, "alpha-night", "Klub Ucho", starts).await?;
        let event_b = seed_event(pool, bravo, city_id, "bravo-night", "klub  ucho", starts).await?;
        let night_a = place_event_of(pool, event_a).await?.ok_or("no night")?;
        let night_b = place_event_of(pool, event_b).await?.ok_or("no night")?;
        assert_eq!(night_a, night_b, "one venue + one date is one night");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM place_events")
                .fetch_one(pool)
                .await?,
            1
        );
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    database.drop_database().await;
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_moved_show_rekeys_the_night() -> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = async {
        let pool = &database.pool;
        let city_id = seed_city(pool).await?;
        let alpha = seed_workspace(pool, "night-alpha").await?;
        let starts = OffsetDateTime::now_utc() + time::Duration::days(14);
        let event_id = seed_event(pool, alpha, city_id, "the-show", "Klub Ucho", starts).await?;
        let first = place_event_of(pool, event_id).await?.ok_or("no night")?;

        // Move the show a day later — same room, new date, new night.
        sqlx::query("UPDATE events SET starts_at = $1 WHERE id = $2")
            .bind(starts + time::Duration::days(1))
            .bind(event_id)
            .execute(pool)
            .await?;
        let moved = place_event_of(pool, event_id)
            .await?
            .ok_or("night lost on move")?;
        assert_ne!(moved, first, "a moved show re-keys the night");
        // The old night held only this event — it is nobody's night now.
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM place_events WHERE id = $1")
                .bind(first)
                .fetch_one(pool)
                .await?,
            0,
            "a night with no events is deleted"
        );

        // A different tenant's show on the *old* date keeps the old night
        // alive — the identity belongs to the room, not the event.
        let bravo = seed_workspace(pool, "night-bravo").await?;
        let other = seed_event(pool, bravo, city_id, "bravo-show", "Klub Ucho", starts).await?;
        sqlx::query("UPDATE events SET starts_at = $1 WHERE id = $2")
            .bind(starts + time::Duration::days(1))
            .bind(other)
            .execute(pool)
            .await?;
        // The old night row is still gone — the new event minted the new
        // date's night fresh; this asserts the moved show did not resurrect
        // or keep the stale one.
        assert_eq!(
            place_event_of(pool, other).await?,
            place_event_of(pool, event_id).await?,
            "both moved shows land on the new night"
        );
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    database.drop_database().await;
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_lenses_hold_their_boundaries() -> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = async {
        let pool = &database.pool;
        let repo = PostgresNightRepository::new(pool.clone());
        let city_id = seed_city(pool).await?;

        // alpha owns the show; bravo is billed on it; charlie is a stranger.
        let alpha = seed_workspace(pool, "night-alpha").await?;
        let bravo = seed_workspace(pool, "night-bravo").await?;
        let charlie = seed_workspace(pool, "night-charlie").await?;
        let starts = OffsetDateTime::now_utc() + time::Duration::days(14);
        let event_id = seed_event(pool, alpha, city_id, "the-show", "Klub Ucho", starts).await?;
        let night = place_event_of(pool, event_id).await?.ok_or("no night")?;
        bill_act(
            pool,
            alpha,
            event_id,
            Some(alpha),
            "alpha-act",
            "Alpha Act",
            0,
        )
        .await?;
        bill_act(
            pool,
            alpha,
            event_id,
            Some(bravo),
            "bravo-act",
            "Bravo Act",
            1,
        )
        .await?;

        // Contributions: alpha's terms and bravo's draw + announce + asks.
        repo.upsert_contribution(
            alpha,
            night,
            ContributionKind::Terms,
            &serde_json::json!({"amount_minor": 125_000, "currency": "PLN"}),
        )
        .await?;
        repo.upsert_contribution(
            bravo,
            night,
            ContributionKind::DrawEstimate,
            &serde_json::json!({"reachable_fans": 640, "expected_draw": 90}),
        )
        .await?;
        repo.upsert_contribution(
            bravo,
            night,
            ContributionKind::AnnounceStatus,
            &serde_json::json!({"state": "announced"}),
        )
        .await?;
        repo.upsert_contribution(
            alpha,
            night,
            ContributionKind::Asks,
            &serde_json::json!({"items": ["share the poster", "tag the venue"]}),
        )
        .await?;
        // A stranger may not contribute — no relationship, no write.
        assert!(matches!(
            repo.upsert_contribution(
                charlie,
                night,
                ContributionKind::Asks,
                &serde_json::json!({"items": ["hello"]}),
            )
            .await,
            Err(NightError::NotFound)
        ));
        // A stranger may not read.
        assert!(matches!(
            repo.load(charlie, night).await,
            Err(NightError::NotFound)
        ));

        // ── OwnBand: the show's owner. ────────────────────────────────
        let own = serde_json::to_value(repo.load(alpha, night).await?)?;
        assert_eq!(own["lens"], "own_band");
        assert_eq!(own["own_event_slug"], "the-show");
        assert_eq!(own["own_terms"]["amount_minor"], 125_000);
        assert_eq!(own["venue"]["display_name"], "Klub Ucho");
        assert!(own["organiser_link"].is_null(), "no link minted yet");
        let co_bill = own["co_bill"].as_array().ok_or("co_bill missing")?;
        assert_eq!(co_bill.len(), 1);
        assert_eq!(co_bill[0]["name"], "Bravo Act");
        assert_eq!(co_bill[0]["confirmed"], false, "not yet confirmed");

        // ── CoBilled: bravo sees the lineup and the public halves. ────
        let billed = serde_json::to_value(repo.load(bravo, night).await?)?;
        assert_eq!(billed["lens"], "co_billed");
        assert_eq!(billed["lineup"].as_array().map(Vec::len), Some(2));
        let announces = billed["public_announce"]
            .as_array()
            .ok_or("no public_announce")?;
        assert_eq!(announces[0]["state"], "announced");
        let asks = billed["asks"].as_array().ok_or("no asks")?;
        assert_eq!(asks[0]["items"].as_array().map(Vec::len), Some(2));
        // The boundary itself: never the parts.
        for forbidden in [
            "own_terms",
            "own_event_slug",
            "contributions",
            "organiser_link",
            "combined_reachable",
            "tickets_sold",
            "payout_total_minor",
            "draw_split",
        ] {
            assert!(
                billed.get(forbidden).is_none(),
                "co_billed must not carry {forbidden}"
            );
        }

        // ── A band confirms itself; the billing tenant cannot. ────────
        assert!(matches!(
            repo.confirm_act(alpha, night, "bravo-act").await,
            Err(NightError::NotFound),
        ));
        repo.confirm_act(bravo, night, "bravo-act").await?;
        let own = serde_json::to_value(repo.load(alpha, night).await?)?;
        assert_eq!(own["co_bill"][0]["confirmed"], true);

        // ── The organiser link: mint once, read the organiser lens. ───
        let link = repo.mint_organiser_link(alpha, night).await?;
        let organiser = serde_json::to_value(repo.load_organiser_by_token(link.token).await?)?;
        assert_eq!(organiser["lens"], "organiser");
        assert_eq!(organiser["combined_reachable"], 640);
        assert_eq!(organiser["payout_total_minor"], 125_000);
        assert_eq!(organiser["tickets_sold"], 0);
        // The proof a band can check: no per-act terms field anywhere in
        // the payload — not "terms", not "amount_minor", not a part.
        let rendered = organiser.to_string();
        for forbidden in [
            "own_terms",
            "terms",
            "amount_minor",
            "currency",
            "co_bill",
            "contributions",
            "organiser_link",
            "roster_acts",
        ] {
            assert!(
                !rendered.contains(forbidden),
                "organiser payload must not contain {forbidden}: {rendered}"
            );
        }
        // Announce is the contributed, public half.
        let announce = organiser["announce"].as_array().ok_or("no announce")?;
        assert_eq!(announce.len(), 1);
        assert_eq!(announce[0]["act"], "Bravo Act");

        // Revocation is real: the dead link reads as a 404.
        repo.revoke_organiser_link(alpha, night).await?;
        assert!(matches!(
            repo.load_organiser_by_token(link.token).await,
            Err(NightError::NotFound)
        ));
        // And a contribution revoked stops contributing.
        repo.revoke_contribution(bravo, night, ContributionKind::DrawEstimate)
            .await?;
        let link2 = repo.mint_organiser_link(bravo, night).await?;
        let organiser = serde_json::to_value(repo.load_organiser_by_token(link2.token).await?)?;
        assert!(
            organiser["combined_reachable"].is_null(),
            "a revoked draw estimate leaves null, not 0"
        );
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    database.drop_database().await;
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_roster_reads_its_own_split() -> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = async {
        let pool = &database.pool;
        let repo = PostgresNightRepository::new(pool.clone());
        let city_id = seed_city(pool).await?;
        let alpha = seed_workspace(pool, "night-alpha").await?;
        let roster = seed_workspace(pool, "night-roster").await?;
        let starts = OffsetDateTime::now_utc() + time::Duration::days(14);
        let event_id = seed_event(pool, alpha, city_id, "the-show", "Klub Ucho", starts).await?;
        let night = place_event_of(pool, event_id).await?.ok_or("no night")?;
        // Two of the roster's acts on one bill — the roster rule.
        bill_act(
            pool,
            alpha,
            event_id,
            Some(roster),
            "roster-one",
            "Roster One",
            0,
        )
        .await?;
        bill_act(
            pool,
            alpha,
            event_id,
            Some(roster),
            "roster-two",
            "Roster Two",
            1,
        )
        .await?;
        bill_act(
            pool,
            alpha,
            event_id,
            Some(alpha),
            "alpha-act",
            "Alpha Act",
            2,
        )
        .await?;

        repo.upsert_contribution(
            roster,
            night,
            ContributionKind::DrawEstimate,
            &serde_json::json!({"reachable_fans": 300}),
        )
        .await?;
        repo.upsert_contribution(
            alpha,
            night,
            ContributionKind::DrawEstimate,
            &serde_json::json!({"reachable_fans": 900}),
        )
        .await?;

        let view = serde_json::to_value(repo.load(roster, night).await?)?;
        assert_eq!(view["lens"], "roster");
        assert_eq!(view["roster_acts"].as_array().map(Vec::len), Some(2));
        assert_eq!(view["draw_split"]["ours"], 300);
        assert_eq!(view["draw_split"]["rest"], 900);
        // Roster is OwnBand plus its additions — the band-side fields land.
        assert!(view.get("co_bill").is_some());
        assert!(view.get("organiser_link").is_some());
        // And the organiser's fields never do.
        for forbidden in [
            "combined_reachable",
            "tickets_sold",
            "payout_total_minor",
            "announce",
        ] {
            assert!(
                view.get(forbidden).is_none(),
                "roster must not carry {forbidden}"
            );
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    database.drop_database().await;
    result
}
