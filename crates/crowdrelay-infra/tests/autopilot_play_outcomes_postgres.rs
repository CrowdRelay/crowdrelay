//! Play measurement against a real Postgres.
//!
//! The rule itself is unit-tested. What cannot be: that the baseline is frozen
//! in the transaction that creates the play, that the window is read as of its
//! own end rather than the worker's clock, that a claim which cannot be made is
//! *stored* as insufficient with its reason rather than silently left blank,
//! and that the schema refuses the shapes the rule is not allowed to produce.

use std::time::Duration;

use crowdrelay_application::autopilot::{
    AutopilotControlRepository, AutopilotDecisionRepository, AutopilotPlayLedgerRepository,
    AutopilotPlayOutcomeRepository, PlayStart, PlayStepPlan, assess_play_claim,
};
use crowdrelay_domain::{
    EventId, WorkspaceId,
    play_measurement::{PlayClaim, PlayMeasurementPolicy},
    plays::{PlayKind, step_schedule},
};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;
use uuid::Uuid;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    event_id: EventId,
    now: OffsetDateTime,
}

async fn fixture(label: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
    let database_url =
        std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").map_err(|error| {
            format!(
                "CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL must target a disposable database: {error}"
            )
        })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("{label}-{suffix}"))
        .bind("Play outcome E2E")
        .execute(&pool)
        .await?;

    let now = OffsetDateTime::now_utc();
    let event_id = EventId::new();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,$4,$5,'published',now())",
    )
    .bind(event_id.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(format!("{label}-show-{suffix}"))
    .bind("Play outcome show")
    .bind(now + time::Duration::days(30))
    .execute(&pool)
    .await?;

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(1),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);
    Ok(Fixture {
        pool,
        repository,
        workspace_id,
        event_id,
        now,
    })
}

/// A daily tracker series climbing at `per_day` from `from_days` to `to_days`,
/// measured in days either side of `now`.
async fn seed_series(
    fixture: &Fixture,
    series_id: Uuid,
    from_days: i64,
    to_days: i64,
    start_value: i64,
    per_day: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    for day in from_days..=to_days {
        sqlx::query(
            "INSERT INTO viryaos_growth_metric_points (workspace_id, series_id, captured_at, value, source)
             VALUES ($1,$2,$3,$4,'test')
             ON CONFLICT (workspace_id, series_id, captured_at) DO NOTHING",
        )
        .bind(fixture.workspace_id.into_uuid())
        .bind(series_id)
        .bind(fixture.now + time::Duration::days(day))
        .bind(start_value + (day - from_days) * per_day)
        .execute(&fixture.pool)
        .await?;
    }
    Ok(())
}

async fn create_series(fixture: &Fixture) -> Result<Uuid, Box<dyn std::error::Error>> {
    let series_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_growth_metric_series (id, workspace_id, platform, metric_key, display_name)
         VALUES ($1,$2,'bandsintown','trackers','Bandsintown trackers')",
    )
    .bind(series_id)
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await?;
    Ok(series_id)
}

fn play_start(fixture: &Fixture, window_days: i64) -> PlayStart {
    let anchor_at = fixture.now + time::Duration::days(30);
    PlayStart {
        kind: PlayKind::TrackUsAsk,
        event_id: fixture.event_id,
        anchor_at,
        hypothesis: PlayKind::TrackUsAsk.hypothesis(),
        success_metric_platform: PlayKind::TrackUsAsk.success_metric().0,
        success_metric_key: PlayKind::TrackUsAsk.success_metric().1,
        steps: PlayKind::TrackUsAsk
            .steps()
            .iter()
            .map(|spec| {
                let (due_at, expires_at) = step_schedule(*spec, anchor_at);
                PlayStepPlan {
                    index: spec.index,
                    kind: spec.kind,
                    class: spec.class,
                    due_at,
                    expires_at,
                }
            })
            .collect(),
        measurement_window_end: fixture.now + time::Duration::days(window_days),
    }
}

/// Marks one fan as reached by the play's first step, so the outcome has a
/// denominator.
async fn record_reach(fixture: &Fixture) -> Result<(), Box<dyn std::error::Error>> {
    let suffix = fixture.workspace_id.into_uuid().simple().to_string();
    let fan_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status)
         VALUES ($1,$2,$3,'active')",
    )
    .bind(fan_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(format!("reached-{suffix}@example.test"))
    .execute(&fixture.pool)
    .await?;
    let step_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM viryaos_play_steps WHERE workspace_id=$1 AND step_index=0 LIMIT 1",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_play_step_recipients (workspace_id, step_id, fan_id, action_id)
         VALUES ($1,$2,$3,$4)",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(step_id)
    .bind(fan_id)
    .bind(Uuid::now_v7())
    .execute(&fixture.pool)
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_play_settles_one_correlational_verdict_and_refuses_the_attributed_claim()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("outcome-e2e").await?;
    let series_id = create_series(&fixture).await?;
    // Fourteen days of one new tracker a day before the play: the pre-play rate.
    seed_series(&fixture, series_id, -14, 0, 500, 1).await?;

    let start = play_start(&fixture, 10);
    assert!(
        fixture
            .repository
            .start_play(fixture.workspace_id, &start)
            .await?
    );

    let outcomes = sqlx::query_as::<_, (String, Option<i64>, Option<i64>, String)>(
        "SELECT claim, baseline_value, baseline_milli_per_day, status
         FROM viryaos_play_outcomes WHERE workspace_id=$1 ORDER BY claim",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_all(&fixture.pool)
    .await?;
    assert_eq!(
        outcomes.len(),
        2,
        "both claims are opened when the play starts, including the one nothing can satisfy"
    );
    for outcome in &outcomes {
        assert_eq!(outcome.3, "pending");
        assert_eq!(
            outcome.1,
            Some(514),
            "the baseline level is frozen at play start"
        );
        assert_eq!(
            outcome.2,
            Some(1_000),
            "one tracker a day, in milli-units per day"
        );
    }

    record_reach(&fixture).await?;
    // Ten days of five a day after it.
    seed_series(&fixture, series_id, 1, 10, 519, 5).await?;

    // Nothing is due until the window closes.
    assert!(
        fixture
            .repository
            .claim_due_play_outcomes(fixture.workspace_id, 8, fixture.now)
            .await?
            .is_empty(),
        "an open window must not be measured mid-flight"
    );

    let after = fixture.now + time::Duration::days(11);
    let claimed = fixture
        .repository
        .claim_due_play_outcomes(fixture.workspace_id, 8, after)
        .await?;
    assert_eq!(claimed.len(), 2);
    for outcome in &claimed {
        let observation = fixture
            .repository
            .observe_play_outcome(fixture.workspace_id, outcome, after)
            .await?;
        assert_eq!(
            observation.recipients_reached, 1,
            "the denominator is who was actually reached"
        );
        assert_eq!(
            observation.observed_at, outcome.window_end,
            "the window is read as of its own end, not as of the worker's clock"
        );
        assert_eq!(
            observation.attributed_clicks, None,
            "no play mints a tracked link yet, and that is not the same as zero clicks"
        );
        let verdict = assess_play_claim(outcome, &observation, PlayMeasurementPolicy::default());
        fixture
            .repository
            .complete_play_outcome(fixture.workspace_id, outcome, &observation, verdict, after)
            .await?;
    }

    let settled = sqlx::query_as::<_, (String, String, Option<String>, Option<String>, Option<i32>, Option<i32>)>(
        "SELECT claim, evidence, evidence_reason, effect_assessment, delta_basis_points, recipients_reached
         FROM viryaos_play_outcomes WHERE workspace_id=$1 ORDER BY claim",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_all(&fixture.pool)
    .await?;
    let attributed = settled
        .iter()
        .find(|row| row.0 == PlayClaim::Attributed.as_str())
        .ok_or("the attributed claim is stored")?;
    assert_eq!(attributed.1, "insufficient");
    assert_eq!(attributed.2.as_deref(), Some("no_attribution_key"));
    assert_eq!(
        attributed.3, None,
        "an unanswerable claim carries no verdict"
    );

    let correlational = settled
        .iter()
        .find(|row| row.0 == PlayClaim::Correlational.as_str())
        .ok_or("the correlational claim is stored")?;
    assert_eq!(correlational.1, "measured");
    assert_eq!(correlational.2, None);
    assert_eq!(correlational.3.as_deref(), Some("improved"));
    assert_eq!(
        correlational.4,
        Some(40_000),
        "five a day against a pre-play one a day"
    );
    assert_eq!(correlational.5, Some(1));

    // And the ledger carries the strength of each claim with the number.
    let ledger = fixture
        .repository
        .load_play_ledger(fixture.workspace_id, after)
        .await?;
    let entry = ledger.plays.first().ok_or("the play is in the ledger")?;
    assert_eq!(entry.recipients_reached, 1);
    assert_eq!(entry.claims.len(), 2);
    assert!(
        entry
            .claims
            .iter()
            .all(|claim| !claim.claim_means.is_empty()),
        "no number leaves without saying what it proves"
    );
    // The standings travel with the ledger, so a kind that stopped appearing
    // can be told from one that retired itself.
    assert!(!ledger.standings.is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_campaign_that_reached_nobody_settles_as_a_non_event()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("outcome-empty").await?;
    let series_id = create_series(&fixture).await?;
    seed_series(&fixture, series_id, -14, 10, 500, 1).await?;

    let start = play_start(&fixture, 10);
    assert!(
        fixture
            .repository
            .start_play(fixture.workspace_id, &start)
            .await?
    );

    let after = fixture.now + time::Duration::days(11);
    for outcome in fixture
        .repository
        .claim_due_play_outcomes(fixture.workspace_id, 8, after)
        .await?
    {
        let observation = fixture
            .repository
            .observe_play_outcome(fixture.workspace_id, &outcome, after)
            .await?;
        let verdict = assess_play_claim(&outcome, &observation, PlayMeasurementPolicy::default());
        fixture
            .repository
            .complete_play_outcome(fixture.workspace_id, &outcome, &observation, verdict, after)
            .await?;
    }

    let reasons = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        "SELECT evidence, evidence_reason, effect_assessment
         FROM viryaos_play_outcomes WHERE workspace_id=$1",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_all(&fixture.pool)
    .await?;
    assert_eq!(reasons.len(), 2);
    for (evidence, reason, assessment) in reasons {
        assert_eq!(evidence, "insufficient");
        assert_eq!(
            reason.as_deref(),
            Some("nothing_delivered"),
            "a campaign that did not run is not a campaign that did not work"
        );
        assert_eq!(assessment, None);
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_schema_refuses_a_verdict_without_evidence() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("outcome-schema").await?;
    let start = play_start(&fixture, 10);
    assert!(
        fixture
            .repository
            .start_play(fixture.workspace_id, &start)
            .await?
    );

    // A verdict with no evidence behind it.
    let orphan_verdict = sqlx::query(
        "UPDATE viryaos_play_outcomes SET effect_assessment='improved' WHERE workspace_id=$1",
    )
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await;
    assert!(
        orphan_verdict.is_err(),
        "an assessment without evidence is the shape a coincidence becomes a cause in"
    );

    // An insufficiency with no reason.
    let silent_gap = sqlx::query(
        "UPDATE viryaos_play_outcomes SET status='succeeded', evidence='insufficient'
         WHERE workspace_id=$1",
    )
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await;
    assert!(
        silent_gap.is_err(),
        "\"we could not tell\" and \"we did not look\" must not be the same row"
    );

    // A settled row with no evidence at all.
    let settled_blank =
        sqlx::query("UPDATE viryaos_play_outcomes SET status='succeeded' WHERE workspace_id=$1")
            .bind(fixture.workspace_id.into_uuid())
            .execute(&fixture.pool)
            .await;
    assert!(
        settled_blank.is_err(),
        "a succeeded measurement that recorded nothing is not a measurement"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_settled_outcome_is_folded_into_the_record_for_its_kind()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("outcome-learning").await?;
    let series_id = create_series(&fixture).await?;
    seed_series(&fixture, series_id, -14, 0, 500, 1).await?;

    let start = play_start(&fixture, 10);
    assert!(
        fixture
            .repository
            .start_play(fixture.workspace_id, &start)
            .await?
    );
    record_reach(&fixture).await?;
    seed_series(&fixture, series_id, 1, 10, 519, 5).await?;

    let after = fixture.now + time::Duration::days(11);
    for outcome in fixture
        .repository
        .claim_due_play_outcomes(fixture.workspace_id, 8, after)
        .await?
    {
        let observation = fixture
            .repository
            .observe_play_outcome(fixture.workspace_id, &outcome, after)
            .await?;
        let verdict = assess_play_claim(&outcome, &observation, PlayMeasurementPolicy::default());
        fixture
            .repository
            .complete_play_outcome(fixture.workspace_id, &outcome, &observation, verdict, after)
            .await?;
    }

    let record = sqlx::query_as::<_, (i32, i32, i32, i32, i32, Option<String>)>(
        "SELECT improved_count, neutral_count, worsened_count, insufficient_count,
                consecutive_worsened, retired_reason
         FROM viryaos_play_learning WHERE workspace_id=$1 AND play_kind='track_us_ask'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        record.0, 1,
        "the correlational verdict is the play's verdict"
    );
    assert_eq!(
        record.3, 0,
        "the attributed claim settled insufficient and must not count"
    );
    assert_eq!(record.4, 0);
    assert_eq!(record.5, None);

    // The standings travel with the ledger, and a single good result leaves the
    // play untested rather than promoted.
    let ledger = fixture
        .repository
        .load_play_ledger(fixture.workspace_id, after)
        .await?;
    let standing = ledger
        .standings
        .first()
        .ok_or("every kind is reported, even without a record")?;
    assert_eq!(standing.record.improved, 1);
    assert_eq!(
        standing.effective_max_recipients_per_step,
        crowdrelay_domain::plays::PlayPolicy::default().max_recipients_per_step,
        "one result changes nothing"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_schema_refuses_a_silent_stop() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("learning-schema").await?;
    // A zero weight with no retirement behind it would stop a play in a way no
    // read model could explain.
    let silent = sqlx::query(
        "INSERT INTO viryaos_play_learning (workspace_id, play_kind, weight_basis_points)
         VALUES ($1,'track_us_ask',0)",
    )
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await;
    assert!(silent.is_err());

    // And a retirement without a reason is not a retirement.
    let unexplained = sqlx::query(
        "INSERT INTO viryaos_play_learning (workspace_id, play_kind, weight_basis_points, retired_at)
         VALUES ($1,'track_us_ask',0,now())",
    )
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await;
    assert!(unexplained.is_err());
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_brief_reports_what_moved_and_what_could_not_be_measured()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("brief-sections").await?;
    let series_id = create_series(&fixture).await?;
    seed_series(&fixture, series_id, -14, 0, 500, 1).await?;
    let start = play_start(&fixture, 10);
    assert!(
        fixture
            .repository
            .start_play(fixture.workspace_id, &start)
            .await?
    );
    record_reach(&fixture).await?;
    seed_series(&fixture, series_id, 1, 10, 519, 5).await?;

    let after = fixture.now + time::Duration::days(11);
    for outcome in fixture
        .repository
        .claim_due_play_outcomes(fixture.workspace_id, 8, after)
        .await?
    {
        let observation = fixture
            .repository
            .observe_play_outcome(fixture.workspace_id, &outcome, after)
            .await?;
        let verdict = assess_play_claim(&outcome, &observation, PlayMeasurementPolicy::default());
        fixture
            .repository
            .complete_play_outcome(fixture.workspace_id, &outcome, &observation, verdict, after)
            .await?;
    }

    let brief = fixture
        .repository
        .load_chief_of_staff(fixture.workspace_id, after)
        .await?;
    assert!(
        brief
            .moved
            .iter()
            .any(|movement| movement.claim == "correlational"),
        "a measured claim is a result, and it says which kind of claim it is"
    );
    assert!(
        brief
            .stopped
            .iter()
            .any(|stopped| stopped.kind == "outcome_insufficient"
                && stopped.reason == "no_attribution_key"),
        "a claim nobody could make is a gap, reported with its own reason"
    );
    assert!(
        !brief
            .moved
            .iter()
            .any(|movement| movement.assessment.is_empty()),
        "nothing reaches the moved section without a verdict"
    );
    Ok(())
}
