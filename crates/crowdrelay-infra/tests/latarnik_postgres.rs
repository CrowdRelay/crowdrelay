//! P.1 — one person, two roles.
//!
//! The join is the point and only a database can show it: a promoter and a fan
//! are two tables, and until now nothing said they were the same human. The
//! rules that decide who may be asked live in the domain and are unit-tested
//! there; what is tested here is which rows the read admits, what it counts,
//! and — the part that matters most — that somebody who opted out is never
//! reported as reachable again.

use std::time::Duration;

use crowdrelay_infra::latarnik::dual_role_review;
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
        let name = format!("crowdrelay_latarnik_{}", Uuid::now_v7().simple());
        let mut admin = PgConnection::connect(&base).await?;
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await?;
        drop(admin);
        let (head, _) = base.rsplit_once('/').ok_or("database url has no path")?;
        let url = format!("{head}/{name}");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(10))
            .connect(&url)
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
async fn the_industry_list_is_also_an_audience() -> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = run(&database.pool).await;
    database.drop_database().await;
    result
}

async fn run(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let act = workspace(pool).await?;
    let city = city(pool).await?;

    // Anna books the room, replied to the band once, and was last written to
    // forty days ago. The person this whole feature exists for.
    let anna = beacon(
        pool,
        act,
        city,
        "promoter",
        "Anna",
        "anna@example.test",
        72,
        true,
    )
    .await?;
    replied(pool, act, anna, city, now).await?;
    contacted(
        pool,
        act,
        "anna@example.test",
        "gig_outreach",
        now - time::Duration::days(40),
    )
    .await?;

    // Bogdan writes about records and already gets the dates — he is in, and
    // the read must say so rather than offering to invite him again.
    let bogdan = beacon(
        pool,
        act,
        city,
        "local_press",
        "Bogdan",
        "bogdan@example.test",
        80,
        true,
    )
    .await?;
    replied(pool, act, bogdan, city, now).await?;
    contacted(
        pool,
        act,
        "bogdan@example.test",
        "beacon_outreach",
        now - time::Duration::days(60),
    )
    .await?;
    consented_fan(pool, act, "bogdan@example.test", true).await?;

    // Celina signed up once and unsubscribed. The address is known and she is
    // NOT reachable — the failure this test exists to prevent.
    let celina = beacon(
        pool,
        act,
        city,
        "photographer",
        "Celina",
        "celina@example.test",
        70,
        true,
    )
    .await?;
    replied(pool, act, celina, city, now).await?;
    contacted(
        pool,
        act,
        "celina@example.test",
        "beacon_outreach",
        now - time::Duration::days(90),
    )
    .await?;
    consented_fan(pool, act, "celina@example.test", false).await?;

    // Dawid came off a directory sweep: a decent score, never contacted, never
    // replied. Cold, and cold is never invited.
    beacon(
        pool,
        act,
        city,
        "promoter",
        "Dawid",
        "dawid@example.test",
        95,
        true,
    )
    .await?;

    // Ewa was written to four days ago about a date. Business first.
    let ewa = beacon(
        pool,
        act,
        city,
        "promoter",
        "Ewa",
        "ewa@example.test",
        75,
        true,
    )
    .await?;
    replied(pool, act, ewa, city, now).await?;
    contacted(
        pool,
        act,
        "ewa@example.test",
        "gig_outreach",
        now - time::Duration::days(4),
    )
    .await?;

    let review = dual_role_review(pool, act, now, true).await?;
    assert_eq!(
        review.total, 5,
        "every active contactable beacon is reviewed"
    );
    assert_eq!(
        review.already_hear_the_dates, 1,
        "only Bogdan has live consent"
    );

    let by_name = |name: &str| {
        review
            .contacts
            .iter()
            .find(|contact| contact.display_name == name)
            .unwrap_or_else(|| panic!("{name} missing from the review"))
    };

    let anna_row = by_name("Anna");
    assert!(anna_row.invitable, "Anna: {:?}", anna_row.hold_reason);
    assert!(!anna_row.hears_the_dates);
    assert_eq!(anna_row.days_since_last_contact, Some(40));

    let bogdan_row = by_name("Bogdan");
    assert!(bogdan_row.hears_the_dates);
    assert!(!bogdan_row.invitable);
    assert!(
        bogdan_row
            .hold_reason
            .as_deref()
            .unwrap_or_default()
            .contains("already get the dates"),
        "{:?}",
        bogdan_row.hold_reason
    );

    // The one that matters: an opt-out is known, not reachable, and not
    // silently re-subscribed by being offered the invitation again.
    let celina_row = by_name("Celina");
    assert!(!celina_row.hears_the_dates, "an opt-out is not reachable");
    assert!(celina_row.known_but_not_consented, "the address is known");

    let dawid_row = by_name("Dawid");
    assert!(
        !dawid_row.invitable,
        "a cold contact was offered an invitation"
    );
    assert!(
        dawid_row
            .hold_reason
            .as_deref()
            .unwrap_or_default()
            .contains("no relationship"),
        "{:?}",
        dawid_row.hold_reason
    );

    let ewa_row = by_name("Ewa");
    assert!(!ewa_row.invitable);
    assert!(
        ewa_row
            .hold_reason
            .as_deref()
            .unwrap_or_default()
            .contains("recently"),
        "{:?}",
        ewa_row.hold_reason
    );

    assert_eq!(review.invitable_now, 1, "Anna alone, today");

    // A band with nothing on the calendar has nothing to say, and the read says
    // that rather than offering letters with no reason in them.
    let nothing_on = dual_role_review(pool, act, now, false).await?;
    assert_eq!(nothing_on.invitable_now, 0);
    assert!(
        nothing_on.contacts.iter().any(|contact| contact
            .hold_reason
            .as_deref()
            .unwrap_or_default()
            .contains("nothing concrete")),
        "no row explained that there is nothing to tell them"
    );
    Ok(())
}

async fn workspace(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Latarnik test')")
        .bind(id)
        .bind(format!("latarnik-{}", id.simple()))
        .execute(pool)
        .await?;
    Ok(id)
}

async fn city(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ($1, 'Wrocław', 'PL', 51.1, 17.0) RETURNING id",
    )
    .bind(format!("wroclaw-{}", Uuid::now_v7().simple()))
    .fetch_one(pool)
    .await?)
}

#[allow(clippy::too_many_arguments)]
async fn beacon(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    kind: &str,
    name: &str,
    email: &str,
    score: i32,
    accepts: bool,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO viryaos_beacons
            (workspace_id, city_id, beacon_kind, display_name, contact_email,
             active, verified, accepts_outreach, relationship_score)
        VALUES ($1, $2, $3, $4, $5, true, true, $6, $7)
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(kind)
    .bind(name)
    .bind(email)
    .bind(accepts)
    .bind(score)
    .fetch_one(pool)
    .await?)
}

/// A beacon campaign that got an answer — the reply that outranks a score.
async fn replied(
    pool: &PgPool,
    workspace_id: Uuid,
    beacon_id: Uuid,
    city_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let event_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO events
            (workspace_id, slug, title, status, starts_at, timezone, city_id, published_at)
        VALUES ($1, $2, 'Test night', 'published', $3, 'Europe/Warsaw', $4, now())
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(format!("night-{}", Uuid::now_v7().simple()))
    .bind(now + time::Duration::days(30))
    .bind(city_id)
    .fetch_one(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO viryaos_beacon_campaigns
            (workspace_id, beacon_id, event_id, status, last_reply_disposition)
        VALUES ($1, $2, $3, 'contacted', 'received')
        "#,
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .bind(event_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The governor row: the band reached this address, in whatever role.
async fn contacted(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
    context: &str,
    at: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        r#"
        INSERT INTO viryaos_contact_governor
            (workspace_id, normalized_contact, last_context, last_outbound_at, next_contact_after)
        VALUES ($1, $2, $3, $4, $4)
        "#,
    )
    .bind(workspace_id)
    .bind(email)
    .bind(context)
    .bind(at)
    .execute(pool)
    .await?;
    Ok(())
}

/// A fan row with the newest marketing consent granted or withdrawn.
async fn consented_fan(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
    granted: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let fan_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO fans (workspace_id, normalized_email, status)
         VALUES ($1, $2, 'active') RETURNING id",
    )
    .bind(workspace_id)
    .bind(email)
    .fetch_one(pool)
    .await?;
    // An opt-out is recorded as a newer row, never as a deletion: the read has
    // to take the newest record, not any record.
    sqlx::query(
        r#"
        INSERT INTO fan_consents
            (workspace_id, fan_id, purpose, granted, policy_version, source, recorded_at)
        VALUES ($1, $2, 'marketing', true, 'privacy-v1', 'test', now() - INTERVAL '10 days')
        "#,
    )
    .bind(workspace_id)
    .bind(fan_id)
    .execute(pool)
    .await?;
    if !granted {
        sqlx::query(
            r#"
            INSERT INTO fan_consents
                (workspace_id, fan_id, purpose, granted, policy_version, source, recorded_at)
            VALUES ($1, $2, 'marketing', false, 'privacy-v1', 'test', now())
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .execute(pool)
        .await?;
    }
    Ok(())
}
