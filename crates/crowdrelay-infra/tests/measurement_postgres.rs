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

mod common;

use crowdrelay_domain::measurement::Measure;
use crowdrelay_infra::measurement_queries;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

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

async fn seed_member(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    sqlx::query_scalar(
        "INSERT INTO workspace_members (workspace_id, normalized_email, display_name, role, status) \
         VALUES ($1, $2, 'Crew Test', 'staff', 'active') RETURNING id",
    )
    .bind(workspace_id)
    .bind(format!("crew-{}@example.test", Uuid::now_v7().simple()))
    .fetch_one(pool)
    .await
    .map_err(Into::into)
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
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run(&database).await
}

async fn run(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = seed_workspace(pool).await?;
    let now = OffsetDateTime::now_utc();

    // ── room_leak: per show, floor 1, NULL capacity is unmeasured. ─────────
    //
    // One completed show in a 300-capacity room with three distinct fans
    // scanned (one fan scanned twice — the second scan is the same
    // (workspace, event, fan) row and the UNIQUE refuses it, so the write is
    // retried with DO NOTHING, which is exactly what the dedupe means), and a
    // second completed show with no room capacity on record at all.
    //
    // The first show also issues a 40-pass pool, and the pool is the point:
    // room size used to be `sum(admission_pools.capacity)`, so this show
    // reported a forty-person room and a 750 bp leak rate. It is a
    // three-hundred-person room that drew three people. The pool stays in the
    // fixture so a regression back to pass-counting fails here.
    let first_show = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO events (workspace_id, slug, title, venue, starts_at, status, room_capacity) \
         VALUES ($1, 'leaky-show', 'Leaky Show', 'Klub Y', now() - interval '10 days', 'completed', 300) \
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
            denominator: 300,
            basis_points: 100,
        },
        "three distinct scans in a three-hundred-capacity room is 100 bp — the \
         forty-pass pool on this show must not be mistaken for the room"
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
        "a show with no room capacity is unmeasured, not zero"
    );
    // The top line sums only the shows that could be judged: 3 scans over a
    // 300-capacity room, and 300 clears the shared floor.
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
            denominator: 300,
            basis_points: 100,
        },
        "the show with no room capacity must not dilute the rate"
    );

    // When every completed show in the window lacks a room capacity the top
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

    // ── drift_caught: days between a slipped due date and the first ────────
    //    reminder that told someone. Two told assignments (gaps of 2 and 4
    //    days → median 3.0) and one open, past-due assignment nobody has been
    //    told about — the breakdown row that says how bad the silence is.
    let drift_ws = seed_workspace(pool).await?;
    let member = seed_member(pool, drift_ws).await?;
    for (days_overdue, told_after_days) in [(10_i64, Some(2_i64)), (6, Some(4)), (3, None)] {
        let due = OffsetDateTime::now_utc() - time::Duration::days(days_overdue);
        sqlx::query(
            "INSERT INTO viryaos_team_assignments
                 (id, workspace_id, source_kind, source_id, assignee_member_id,
                  required_skill, status, due_at, first_overdue_reminder_at)
             VALUES ($1, $2, 'show_task', $3, $4, 'video', 'open', $5, $6)",
        )
        .bind(Uuid::now_v7())
        .bind(drift_ws)
        .bind(Uuid::now_v7())
        .bind(member)
        .bind(due)
        .bind(told_after_days.map(|days| due + time::Duration::days(days)))
        .execute(pool)
        .await?;
    }
    let drift = sqlx::query(measurement_queries::DRIFT_CAUGHT_SQL)
        .bind(drift_ws)
        .bind(OffsetDateTime::now_utc())
        .fetch_one(pool)
        .await?;
    let (n, median_days, slipped_untold): (i64, Option<f64>, i64) = {
        use sqlx::Row;
        (
            drift.try_get("n")?,
            drift.try_get("median_days")?,
            drift.try_get("slipped_untold")?,
        )
    };
    assert_eq!(n, 2, "only the two told assignments count");
    assert!(
        (median_days.ok_or("median over two rows is never NULL")? - 3.0).abs() < 1e-9,
        "median of 2 and 4 days is 3.0"
    );
    assert_eq!(slipped_untold, 1, "the untold slip is the honest number");

    // ── counterparty_pull: delivered T+7 reports → follow-up ───────────────
    //    conversations. Two delivered reports; the first counterparty wrote
    //    back after it landed, the second's last inbound predates the report
    //    and is not a conversation. 1/2 → 5000 basis points at floor 1.
    let pull_ws = seed_workspace(pool).await?;
    for email in ["a@x.pl", "b@x.pl"] {
        sqlx::query(
            "INSERT INTO outbox_events (workspace_id, event_type, payload, status, delivered_at) \
             VALUES ($1, 'crowdrelay.show.post_show_report_due', $2, 'delivered', \
                     now() - interval '5 days')",
        )
        .bind(pull_ws)
        .bind(serde_json::json!({"report": {"counterparty": {"email": email}}}))
        .execute(pool)
        .await?;
    }
    for (email, last_inbound) in [
        (
            "a@x.pl",
            OffsetDateTime::now_utc() - time::Duration::days(3),
        ),
        (
            "b@x.pl",
            OffsetDateTime::now_utc() - time::Duration::days(10),
        ),
    ] {
        sqlx::query(
            "INSERT INTO viryaos_drive_contacts
                 (workspace_id, normalized_email, source_file_id, source_file_name, last_inbound_at)
             VALUES ($1, $2, 'msg-1', 'gmail sync', $3)",
        )
        .bind(pull_ws)
        .bind(email)
        .bind(last_inbound)
        .execute(pool)
        .await?;
    }
    let pull = sqlx::query(measurement_queries::COUNTERPARTY_PULL_SQL)
        .bind(pull_ws)
        .bind(OffsetDateTime::now_utc())
        .fetch_one(pool)
        .await?;
    let (delivered, conversations): (i64, i64) = {
        use sqlx::Row;
        (pull.try_get("delivered")?, pull.try_get("conversations")?)
    };
    assert_eq!((delivered, conversations), (2, 1));
    assert_eq!(
        Measure::rate_with_floor(conversations, delivered, 1),
        Measure::Rate {
            numerator: 1,
            denominator: 2,
            basis_points: 5_000,
        },
        "one answered report out of two delivered is stated at floor 1"
    );

    // `record_inbound_sighting` is monotonic: an earlier `at` never lowers
    // the stored stamp.
    let repo = crowdrelay_infra::gdrive::PostgresGDriveRepository::new(pool.clone());
    let earlier = OffsetDateTime::now_utc() - time::Duration::days(9);
    repo.record_inbound_sighting(pull_ws, "a@x.pl", earlier)
        .await?;
    let stored: OffsetDateTime = sqlx::query_scalar(
        "SELECT last_inbound_at FROM viryaos_drive_contacts \
         WHERE workspace_id = $1 AND normalized_email = 'a@x.pl'",
    )
    .bind(pull_ws)
    .fetch_one(pool)
    .await?;
    assert!(
        stored > earlier,
        "an earlier sighting must not lower last_inbound_at"
    );

    Ok(())
}
