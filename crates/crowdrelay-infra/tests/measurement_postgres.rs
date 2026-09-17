//! The measurement ledger's queries, driven against a real schema.
//!
//! Every claim in `crowdrelay_infra::measurement_queries` names columns the
//! compiler never checked — SQLx runs queries at runtime here by design — so
//! a wrong column is a 503 on first request, and the numbers themselves are
//! the point: a checkin counted twice, a recovered archive contact counted as
//! growth, or a five-row sample stating a rate are all wrong in ways only a
//! real database can show.
//!
//! Three cases: the room-leak per-show rule (floor 1, NULL capacity is
//! unmeasured rather than zero), the growth/recovery split on
//! `fan_import:` first touches, and the rate floor refusing to state a rate
//! on five observations.

use crowdrelay_domain::measurement::Measure;
use crowdrelay_infra::measurement_queries;
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
        let name = format!("crowdrelay_measure_{}", Uuid::now_v7().simple());
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

async fn seed_fan(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status) VALUES ($1, $2, $3, 'active')",
    )
    .bind(id)
    .bind(workspace_id)
    .bind(email)
    .execute(pool)
    .await?;
    Ok(id)
}

/// The latest `fan_consents` row wins. `granted` on the newest row is the
/// consent state; an earlier granted row followed by a revocation reads as
/// revoked, which is the whole point of the `latest_marketing` CTE.
async fn record_marketing_consent(
    pool: &PgPool,
    workspace_id: Uuid,
    fan_id: Uuid,
    granted: bool,
    recorded_at: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO fan_consents (workspace_id, fan_id, purpose, granted, policy_version, source, recorded_at) \
         VALUES ($1, $2, 'marketing', $3, 'privacy-v1', 'test', $4)",
    )
    .bind(workspace_id)
    .bind(fan_id)
    .bind(granted)
    .bind(recorded_at)
    .execute(pool)
    .await?;
    Ok(())
}

async fn record_first_touch(
    pool: &PgPool,
    workspace_id: Uuid,
    fan_id: Uuid,
    source: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO fan_acquisition_events (workspace_id, fan_id, source, request_id, occurred_at) \
         VALUES ($1, $2, $3, $4, now() - interval '1 day')",
    )
    .bind(workspace_id)
    .bind(fan_id)
    .bind(source)
    .bind(format!("req-{}", Uuid::now_v7().simple()))
    .execute(pool)
    .await?;
    Ok(())
}

/// The per-show rule the read model applies to a `ROOM_LEAK_SQL` row: floor 1,
/// because one show is one observation and the plan's own words are
/// "per show". A show with no admission capacity on record is unmeasured.
fn per_show(scans: i64, room_size: Option<i64>) -> Measure {
    match room_size {
        Some(room) if room > 0 => Measure::Rate {
            numerator: scans,
            denominator: room,
            basis_points: u16::try_from(scans * 10_000 / room).unwrap_or(10_000),
        },
        _ => Measure::Unmeasured {
            reason: "no admission capacity on record for this show",
        },
    }
}

#[derive(Debug, sqlx::FromRow)]
struct ShowRow {
    slug: String,
    scans: i64,
    room_size: Option<i64>,
}

#[derive(Debug, sqlx::FromRow)]
struct ChannelFansRow {
    channel: String,
    fans: i64,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_measurement_ledger_counts_what_the_plan_counts()
-> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = run(&database.pool).await;
    database.drop_database().await;
    result
}

async fn run(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = seed_workspace(pool).await?;
    let now = OffsetDateTime::now_utc();

    // ── room_leak: per show, floor 1, NULL capacity is unmeasured. ─────────
    //
    // One completed show with a 40-capacity pool and three distinct fans
    // scanned (one fan scanned twice — the second scan is the same
    // (workspace, event, fan) row and the UNIQUE refuses it, so the write is
    // retried with DO NOTHING, which is exactly what the dedupe means), and a
    // second completed show with no admission pool at all.
    let first_show = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO events (workspace_id, slug, title, venue, starts_at, status) \
         VALUES ($1, 'leaky-show', 'Leaky Show', 'Klub Y', now() - interval '10 days', 'completed') \
         RETURNING id",
    )
    .bind(workspace)
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO admission_pools (workspace_id, event_id, name, slug, capacity) \
         VALUES ($1, $2, 'General', 'general', 40)",
    )
    .bind(workspace)
    .bind(first_show)
    .execute(pool)
    .await?;
    let campaign = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO concert_qr_campaigns (workspace_id, event_id, label, valid_from, valid_until) \
         VALUES ($1, $2, 'Door QR', now() - interval '11 days', now() - interval '1 day') \
         RETURNING id",
    )
    .bind(workspace)
    .bind(first_show)
    .fetch_one(pool)
    .await?;
    for index in 0..3 {
        let fan = seed_fan(pool, workspace, &format!("checkin-{index}@example.com")).await?;
        sqlx::query(
            "INSERT INTO concert_checkins (workspace_id, event_id, campaign_id, fan_id) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(workspace)
        .bind(first_show)
        .bind(campaign)
        .bind(fan)
        .execute(pool)
        .await?;
        if index == 0 {
            // The same fan scans again; the table keeps one row per
            // (event, fan), and the query counts DISTINCT fan_id.
            sqlx::query(
                "INSERT INTO concert_checkins (workspace_id, event_id, campaign_id, fan_id) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (workspace_id, event_id, fan_id) DO NOTHING",
            )
            .bind(workspace)
            .bind(first_show)
            .bind(campaign)
            .bind(fan)
            .execute(pool)
            .await?;
        }
    }
    sqlx::query(
        "INSERT INTO events (workspace_id, slug, title, venue, starts_at, status) \
         VALUES ($1, 'unpooled-show', 'Unpooled Show', 'Klub Z', now() - interval '5 days', 'completed')",
    )
    .bind(workspace)
    .execute(pool)
    .await?;

    let shows = sqlx::query_as::<_, ShowRow>(measurement_queries::ROOM_LEAK_SQL)
        .bind(workspace)
        .bind(now)
        .fetch_all(pool)
        .await?;
    assert_eq!(shows.len(), 2, "both completed shows are in the window");
    let leaky = shows
        .iter()
        .find(|row| row.slug == "leaky-show")
        .ok_or("the pooled show is missing")?;
    assert_eq!(
        per_show(leaky.scans, leaky.room_size),
        Measure::Rate {
            numerator: 3,
            denominator: 40,
            basis_points: 750,
        },
        "three distinct scans against a forty-capacity room is 750 bp"
    );
    let unpooled = shows
        .iter()
        .find(|row| row.slug == "unpooled-show")
        .ok_or("the unpooled show is missing")?;
    assert_eq!(
        per_show(unpooled.scans, unpooled.room_size),
        Measure::Unmeasured {
            reason: "no admission capacity on record for this show",
        },
        "a show with no admission pool is unmeasured, not zero"
    );
    // The top line sums only the shows that could be judged: 3 scans over a
    // 40-capacity room, and 40 clears the shared floor.
    let top = Measure::rate(
        shows
            .iter()
            .filter(|row| row.room_size.is_some_and(|room| room > 0))
            .map(|row| row.scans)
            .sum(),
        shows
            .iter()
            .filter_map(|row| row.room_size)
            .filter(|room| *room > 0)
            .sum(),
    );
    assert_eq!(
        top,
        Measure::Rate {
            numerator: 3,
            denominator: 40,
            basis_points: 750,
        },
        "the unpooled show must not dilute the rate"
    );

    // When every completed show in the window lacks an admission pool the top
    // line is unmeasured — "we never knew the room sizes" — not a BelowFloor
    // that would read as "too few shows". A second workspace keeps this case
    // isolated from the pooled shows above.
    let capacity_blind = seed_workspace(pool).await?;
    sqlx::query(
        "INSERT INTO events (workspace_id, slug, title, venue, starts_at, status) \
         VALUES ($1, 'unsized-show', 'Unsized Show', 'Klub Q', now() - interval '3 days', 'completed')",
    )
    .bind(capacity_blind)
    .execute(pool)
    .await?;
    let blind_shows = sqlx::query_as::<_, ShowRow>(measurement_queries::ROOM_LEAK_SQL)
        .bind(capacity_blind)
        .bind(OffsetDateTime::now_utc())
        .fetch_all(pool)
        .await?;
    assert_eq!(blind_shows.len(), 1);
    assert_eq!(
        per_show(blind_shows[0].scans, blind_shows[0].room_size),
        Measure::Unmeasured {
            reason: "no admission capacity on record for this show",
        }
    );
    let blind_measured = blind_shows
        .iter()
        .filter(|row| row.room_size.is_some_and(|room| room > 0))
        .count();
    assert_eq!(blind_measured, 0);
    let blind_top = if blind_measured == 0 {
        Measure::Unmeasured {
            reason: "completed shows in the window have no admission capacity on record",
        }
    } else {
        Measure::rate(0, 0)
    };
    assert_eq!(
        blind_top,
        Measure::Unmeasured {
            reason: "completed shows in the window have no admission capacity on record",
        },
        "no measurable show is unmeasured, not a rate the floor hides"
    );

    // ── fans_gathered vs recovery_not_growth: the fan_import split. ────────
    //
    // Two fans whose first touch is an archive import, three whose first
    // touch is a door QR, and one door-QR fan whose marketing consent was
    // granted and then revoked — the latest row decides, so six consented
    // fans split 3 gathered / 2 recovered and the revoked one counts nowhere.
    for index in 0..2 {
        let fan = seed_fan(pool, workspace, &format!("archive-{index}@example.com")).await?;
        record_marketing_consent(pool, workspace, fan, true, now - time::Duration::days(2)).await?;
        record_first_touch(pool, workspace, fan, "fan_import:batch-1").await?;
    }
    for index in 0..3 {
        let fan = seed_fan(pool, workspace, &format!("qr-{index}@example.com")).await?;
        record_marketing_consent(pool, workspace, fan, true, now - time::Duration::days(2)).await?;
        record_first_touch(pool, workspace, fan, "qr:door").await?;
    }
    let revoked = seed_fan(pool, workspace, "revoked@example.com").await?;
    record_marketing_consent(
        pool,
        workspace,
        revoked,
        true,
        now - time::Duration::days(2),
    )
    .await?;
    record_marketing_consent(
        pool,
        workspace,
        revoked,
        false,
        now - time::Duration::days(1),
    )
    .await?;
    record_first_touch(pool, workspace, revoked, "qr:door").await?;

    let gathered = sqlx::query_as::<_, ChannelFansRow>(measurement_queries::FANS_GATHERED_SQL)
        .bind(workspace)
        .bind(OffsetDateTime::now_utc())
        .fetch_all(pool)
        .await?;
    assert_eq!(
        gathered.len(),
        1,
        "only the qr channel should hold gathered fans: {gathered:?}"
    );
    assert_eq!(gathered[0].channel, "qr");
    assert_eq!(gathered[0].fans, 3, "the revoked fan is not gathered");

    let recovered = sqlx::query_scalar::<_, i64>(measurement_queries::RECOVERY_NOT_GROWTH_SQL)
        .bind(workspace)
        .bind(OffsetDateTime::now_utc())
        .fetch_one(pool)
        .await?;
    assert_eq!(recovered, 2, "archive imports are recovery, not growth");

    // ── suggestions_read: the floor refuses a rate on five observations. ───
    //
    // Five suggestions shown, two acted on. 2/5 would read 4000 basis points;
    // the floor reads "too few to call" instead, because five rows is an
    // anecdote, not a rate.
    for (index, status) in ["approved", "done", "raised", "raised", "declined"]
        .iter()
        .enumerate()
    {
        sqlx::query(
            "INSERT INTO viryaos_content_suggestions (id, workspace_id, concept, status) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(Uuid::now_v7())
        .bind(workspace)
        .bind(format!("suggestion {index}"))
        .bind(*status)
        .execute(pool)
        .await?;
    }
    let row = sqlx::query(measurement_queries::SUGGESTIONS_READ_SQL)
        .bind(workspace)
        .bind(OffsetDateTime::now_utc())
        .fetch_one(pool)
        .await?;
    let (shown, acted): (i64, i64) = {
        use sqlx::Row;
        (row.try_get("shown")?, row.try_get("acted")?)
    };
    assert_eq!((shown, acted), (5, 2));
    assert_eq!(
        Measure::rate(acted, shown),
        Measure::BelowFloor {
            numerator: 2,
            denominator: 5,
            floor: 20,
        },
        "five observations is below the floor — the rate is not stated"
    );

    Ok(())
}
