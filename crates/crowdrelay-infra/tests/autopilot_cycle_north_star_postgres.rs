//! The brain's self-assessment has to be able to reach a verdict.
//!
//! `GET /v1/admin/ops/cycles` reports `brain_state`, and it derived that state
//! from the cycle rows it was about to render. Those default to twenty and cap
//! at two hundred, and a cycle runs every five minutes, so the page covers at
//! most sixteen hours -- one or two distinct days. `self_assessment::assess`
//! needs six before it will claim a trend, so the endpoint answered
//! `initializing` on every request forever, no matter how long the system had
//! been running or which way the fan count was going. That reads as a system
//! being careful and is really a window that can never be filled.
//!
//! The series now comes from its own query over a sixty-day window. What is
//! asserted here is what a unit test cannot reach: that the query collapses a
//! day of five-minute cycles to one observation, keeps the last reading of each
//! day rather than the first, and ignores the outcome filter the list applies --
//! `?state=degraded` must not change the reported health of the brain.
//!
//! The write side is asserted here too. `close_cycle_run` used to derive
//! `north_star_value` itself, as a count of active fans, which is the North Star
//! only for a tenant that has chosen fans -- and the default is signal installs.
//! That put two numbers under one name, and had the self-assessment trending
//! whichever of them the brain was not optimizing. The cycle now records the
//! reading it took, and records nothing when it took none.

use crowdrelay_brain::self_assessment::{BrainState, assess};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::autopilot::{
    CycleTrigger, NORTH_STAR_WINDOW_DAYS, close_cycle_run, daily_north_star, open_cycle_run,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

/// The infra suite carries no `anyhow`; the boxed error is what every other
/// test in this directory uses.
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

async fn pool() -> Result<PgPool> {
    let url = std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL")?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    Ok(pool)
}

async fn workspace(pool: &PgPool) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("cycle-north-star-{}", id.simple()))
        .bind("Cycle North Star")
        .execute(pool)
        .await?;
    Ok(WorkspaceId::from_uuid(id))
}

/// One closed cycle run with a North Star reading.
async fn cycle(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    started_at: OffsetDateTime,
    outcome: &str,
    north_star_value: Option<i32>,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_cycle_runs
            (id, workspace_id, trigger, started_at, finished_at, duration_ms,
             outcome, decisions_recorded, actions_created, north_star_value)
        VALUES ($1,$2,'scheduled',$3,$3,10,$4,0,0,$5)
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(started_at)
    .bind(outcome)
    .bind(north_star_value)
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_growing_fanbase_is_reported_as_improving() -> Result<()> {
    let pool = pool().await?;
    let workspace_id = workspace(&pool).await?;
    let now = OffsetDateTime::now_utc();

    // Eight days of five-minute cycles, the fan count rising across them. Read
    // off the rendered page this is at most two days of history and the brain
    // has no opinion; read off the daily series it is a clear trend.
    for day in 0..8_i64 {
        let value = 10 + (day as i32) * 5;
        for tick in 0..12_i64 {
            cycle(
                &pool,
                workspace_id,
                now - Duration::days(7 - day) + Duration::minutes(tick * 5),
                "succeeded",
                Some(value),
            )
            .await?;
        }
    }

    let samples = daily_north_star(&pool, workspace_id, NORTH_STAR_WINDOW_DAYS).await?;
    assert!(
        samples.len() == 8,
        "a day of five-minute cycles is one observation, so eight days must \
         yield eight samples, got {}",
        samples.len()
    );
    let state = assess(samples);
    assert!(
        state == BrainState::Improving,
        "eight days of a rising fan count must read as improving, got {state:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_last_reading_of_the_day_wins_and_degraded_cycles_still_count() -> Result<()> {
    let pool = pool().await?;
    let workspace_id = workspace(&pool).await?;
    // Fixed at midday so the three readings cannot straddle a date boundary.
    let midday = OffsetDateTime::now_utc()
        .replace_time(time::Time::from_hms(12, 0, 0).expect("a valid time of day"));

    // Fans arrived through the day. The count is cumulative, so the day's
    // result is the last reading, not the first.
    cycle(&pool, workspace_id, midday, "succeeded", Some(10)).await?;
    cycle(
        &pool,
        workspace_id,
        midday + Duration::minutes(5),
        "succeeded",
        Some(14),
    )
    .await?;
    // A degraded cycle still measured the fanbase. The list endpoint can filter
    // these out for display; the assessment must not lose the reading.
    cycle(
        &pool,
        workspace_id,
        midday + Duration::minutes(10),
        "degraded",
        Some(21),
    )
    .await?;
    // A cycle whose reading could not be taken contributes nothing rather than
    // a zero, which would be indistinguishable from having lost every fan.
    cycle(
        &pool,
        workspace_id,
        midday + Duration::minutes(15),
        "degraded",
        None,
    )
    .await?;

    let samples = daily_north_star(&pool, workspace_id, NORTH_STAR_WINDOW_DAYS).await?;
    assert!(
        samples.len() == 1,
        "four cycles in one day are one observation, got {}",
        samples.len()
    );
    let value = samples.first().expect("one sample").value;
    assert!(
        (value - 21.0).abs() < f64::EPSILON,
        "the day's result is its last reading, expected 21, got {value}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn readings_outside_the_window_are_not_read() -> Result<()> {
    let pool = pool().await?;
    let workspace_id = workspace(&pool).await?;
    let now = OffsetDateTime::now_utc();

    cycle(&pool, workspace_id, now, "succeeded", Some(10)).await?;
    cycle(
        &pool,
        workspace_id,
        now - Duration::days(i64::from(NORTH_STAR_WINDOW_DAYS) + 5),
        "succeeded",
        Some(999),
    )
    .await?;

    let samples = daily_north_star(&pool, workspace_id, NORTH_STAR_WINDOW_DAYS).await?;
    assert!(
        samples.len() == 1,
        "only readings inside the window count, got {} samples",
        samples.len()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn another_workspaces_cycles_are_not_read() -> Result<()> {
    let pool = pool().await?;
    let mine = workspace(&pool).await?;
    let theirs = workspace(&pool).await?;
    let now = OffsetDateTime::now_utc();

    for day in 0..8_i64 {
        cycle(
            &pool,
            theirs,
            now - Duration::days(day),
            "succeeded",
            Some(1_000),
        )
        .await?;
    }
    cycle(&pool, mine, now, "succeeded", Some(10)).await?;

    let samples = daily_north_star(&pool, mine, NORTH_STAR_WINDOW_DAYS).await?;
    assert!(
        samples.len() == 1,
        "the series is per workspace, got {} samples",
        samples.len()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_window_is_wide_enough_for_stagnation_to_be_reachable() -> Result<()> {
    let pool = pool().await?;
    let workspace_id = workspace(&pool).await?;
    let now = OffsetDateTime::now_utc();

    // A month of a flat fan count, with a scattering of days on which no
    // reading could be taken. Stagnation needs thirty samples; a window of
    // exactly thirty days would be defeated by the first missing one.
    let mut written = 0;
    for day in 0..40_i64 {
        if day % 7 == 3 {
            continue;
        }
        cycle(
            &pool,
            workspace_id,
            now - Duration::days(day),
            "succeeded",
            Some(10),
        )
        .await?;
        written += 1;
    }
    assert!(written >= 30, "the fixture must supply enough days");

    let state = assess(daily_north_star(&pool, workspace_id, NORTH_STAR_WINDOW_DAYS).await?);
    assert!(
        state == BrainState::Stagnant,
        "a month of a flat fan count, with gaps, must still reach stagnation, got {state:?}"
    );
    assert!(
        state.needs_attention(),
        "stagnation is one of the two states that asks for a human"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_cycle_records_the_reading_it_was_given_not_a_fan_count() -> Result<()> {
    let pool = pool().await?;
    let workspace_id = workspace(&pool).await?;
    let now = OffsetDateTime::now_utc();

    // Three active fans. The cycle record used to compute `north_star_value`
    // itself, as `count(*) FROM fans WHERE status = 'active'`, which is not the
    // North Star for any tenant that has not chosen fans -- and the default is
    // signal installs. That left two numbers under one name: the world model's
    // on `/autopilot/cycle/preview`, this one on `/ops/cycles`, with the
    // self-assessment trending whichever the brain was not optimizing.
    for index in 0..3 {
        sqlx::query(
            "INSERT INTO fans (id, workspace_id, normalized_email, status) VALUES ($1,$2,$3,'active')",
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id.into_uuid())
        .bind(format!("fan-{index}-{}@north-star.test", Uuid::now_v7().simple()))
        .execute(&pool)
        .await?;
    }

    let cycle_id = open_cycle_run(&pool, workspace_id, CycleTrigger::Scheduled, now)
        .await
        .ok_or("the cycle run record must open")?;
    // The reading the world model resolved: signal installs, not the three fans.
    close_cycle_run(&pool, workspace_id, cycle_id, false, now, Some(1)).await;

    let stored: Option<i32> = sqlx::query_scalar(
        "SELECT north_star_value FROM viryaos_autopilot_cycle_runs WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(cycle_id)
    .fetch_one(&pool)
    .await?;
    assert!(
        stored == Some(1),
        "the record must store the reading the cycle took, not a count it \
         derived for itself; expected Some(1), got {stored:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_cycle_that_took_no_reading_records_none() -> Result<()> {
    let pool = pool().await?;
    let workspace_id = workspace(&pool).await?;
    let now = OffsetDateTime::now_utc();

    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status) VALUES ($1,$2,$3,'active')",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!(
        "only-fan-{}@north-star.test",
        Uuid::now_v7().simple()
    ))
    .execute(&pool)
    .await?;

    let cycle_id = open_cycle_run(&pool, workspace_id, CycleTrigger::Scheduled, now)
        .await
        .ok_or("the cycle run record must open")?;
    // The evaluation phase never got far enough to read anything.
    close_cycle_run(&pool, workspace_id, cycle_id, true, now, None).await;

    let stored: Option<i32> = sqlx::query_scalar(
        "SELECT north_star_value FROM viryaos_autopilot_cycle_runs WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(cycle_id)
    .fetch_one(&pool)
    .await?;
    assert!(
        stored.is_none(),
        "a cycle that took no reading records none -- a zero here cannot be \
         told apart from having lost the whole audience; got {stored:?}"
    );
    // And the series must skip it rather than trending it as a collapse.
    let samples = daily_north_star(&pool, workspace_id, NORTH_STAR_WINDOW_DAYS).await?;
    assert!(
        samples.is_empty(),
        "an unread cycle contributes nothing to the series, got {} samples",
        samples.len()
    );
    Ok(())
}
