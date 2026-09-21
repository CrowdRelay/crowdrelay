//! The email loop resolves, end to end, through the worker's own path.
//!
//! "A reply" is not just a promoter answering: silence after the window is
//! itself a resolved outcome, not missing data. These tests drive the real
//! seam — `claim_due_measurements` -> `observe_measurement` ->
//! `assess_measurement_effect` -> `complete_measurement` — against real
//! interaction rows, and assert what the outcome record says.
//!
//! A: an outreach letter nobody answered resolves to a measured 0.0 — the
//!    brain learns "this pitch did not land", it does not wait forever.
//! B: a reply inside the window resolves to 1.0.
//! C: a reply that lands *after* the seven-day window still reads as no
//!    reply — the measurement is about the window, not about whether the
//!    promoter ever wrote back.
//! D: two booking letters to the same promoter, one reply: the reply belongs
//!    to the letter that preceded it. The earlier letter's measurement
//!    resolves to 0.0 — claiming the reply anyway would double-count one
//!    answer as two successes.
//!
//! Requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL — a disposable database,
//! migrated fresh by the fixture.

use crowdrelay_application::autopilot::{
    AutopilotMeasurementKind, AutopilotMeasurementRepository, ClaimedAutopilotMeasurement,
    HarmObservation, assess_measurement_effect,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
}

async fn setup() -> Result<Fixture, Box<dyn std::error::Error>> {
    let database_url = std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").map_err(|e| {
        format!("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL must target a disposable database: {e}")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("reply-resolution-{suffix}"))
        .bind("Reply Resolution Tests")
        .execute(&pool)
        .await?;
    let repository = PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: database_url,
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        },
    );
    Ok(Fixture {
        pool,
        repository,
        workspace_id,
        now: OffsetDateTime::now_utc(),
    })
}

/// A decision, an action and the growth evidence row a dispatch writes —
/// the envelope every executor-confirmed action carries in production.
async fn insert_dispatch(
    f: &Fixture,
    finished_at: OffsetDateTime,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id)
           VALUES ($1,$2,$3,'booking_opportunity','target',$4,
                   'auto_execute',9000,'auto_execute','test',
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,gen_random_uuid())"#,
    )
    .bind(decision_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("key-{action_id}"))
    .bind(action_id)
    .execute(&f.pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, action_class, finished_at)
           VALUES ($1,$2,$3,'booking_opportunity','gig.outreach.send','target',
                   $4,$5,'{}'::jsonb,'succeeded','owned_audience',$6)"#,
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .bind(format!("idem-{action_id}"))
    .bind(finished_at)
    .execute(&f.pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO growth_evidence
           (workspace_id, action_id, opportunity_id, timestamp, recipient_id,
            channel, estimated_reach, treatment, propensity, converted,
            predicted_fans, predicted_signal_installs, context, evidence_quality)
           VALUES ($1,$2,$3,$4,'recipient','email',100,'treatment',0.9,false,
                   1.0,0.5,'{}'::jsonb,'observational')"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(format!("opp-{action_id}"))
    .bind(finished_at)
    .execute(&f.pool)
    .await?;
    Ok(action_id)
}

/// Queues the reply measurement the dispatch would have scheduled, due
/// already — the worker claims it immediately.
async fn queue_reply_measurement(
    f: &Fixture,
    action_id: Uuid,
    target_id: Uuid,
    kind: AutopilotMeasurementKind,
    letter_sent_at: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let due_at = letter_sent_at + time::Duration::days(7);
    sqlx::query(
        r#"INSERT INTO autopilot_measurements
           (id, workspace_id, action_id, measurement_kind, subject_id,
            action_finished_at, baseline_value, due_at, available_at)
           VALUES ($1,$2,$3,$4,$5,$6,0,$7,$7)"#,
    )
    .bind(Uuid::now_v7())
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(kind.as_str())
    .bind(target_id)
    .bind(letter_sent_at)
    .bind(due_at)
    .execute(&f.pool)
    .await?;
    Ok(())
}

async fn outreach_target(f: &Fixture, email: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO outreach_targets (id, workspace_id, target_kind, display_name, contact_email)
         VALUES ($1, $2, 'press', $3, $4)",
    )
    .bind(id)
    .bind(f.workspace_id.into_uuid())
    .bind(email)
    .bind(email)
    .execute(&f.pool)
    .await?;
    Ok(id)
}

async fn booking_target(f: &Fixture, email: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    let city_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO cities (id, slug, name, country_code, latitude, longitude)
         VALUES ($1, $2, $2, 'PL', 51.1, 17.0)",
    )
    .bind(city_id)
    .bind(format!("reply-city-{}", city_id.simple()))
    .execute(&f.pool)
    .await?;
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO booking_targets (id, workspace_id, city_id, target_kind, display_name, contact_email)
         VALUES ($1, $2, $3, 'venue', $4, $5)",
    )
    .bind(id)
    .bind(f.workspace_id.into_uuid())
    .bind(city_id)
    .bind(email)
    .bind(email)
    .execute(&f.pool)
    .await?;
    Ok(id)
}

async fn interaction(
    f: &Fixture,
    table: &str,
    target_id: Uuid,
    direction: &str,
    source_key: &str,
    occurred_at: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let query = format!(
        "INSERT INTO {table} (workspace_id, target_id, direction, phase, source_key, occurred_at)
         VALUES ($1, $2, $3, 'initial', $4, $5)"
    );
    sqlx::query(&query)
        .bind(f.workspace_id.into_uuid())
        .bind(target_id)
        .bind(direction)
        .bind(source_key)
        .bind(occurred_at)
        .execute(&f.pool)
        .await?;
    Ok(())
}

/// Claims the one due measurement, observes it through the worker's own
/// observation path, and completes it. Returns what the outcome row says —
/// the number the brain will actually read.
async fn run_due_measurement(
    f: &Fixture,
    now: OffsetDateTime,
) -> Result<(f64, ClaimedAutopilotMeasurement), Box<dyn std::error::Error>> {
    let claimed = f
        .repository
        .claim_due_measurements(f.workspace_id, 1, now)
        .await?;
    let measurement = claimed.into_iter().next().expect("one due measurement");
    let observed = f
        .repository
        .observe_measurement(f.workspace_id, &measurement, now)
        .await?;
    let effect = assess_measurement_effect(&measurement, observed, &HarmObservation::default())
        .expect("a measurement the worker can classify");
    f.repository
        .complete_measurement(
            f.workspace_id,
            &measurement,
            observed,
            effect,
            Some(&HarmObservation::default()),
            now,
        )
        .await?;
    Ok((observed, measurement))
}

/// The outcome row `complete_measurement` wrote for the action.
async fn outcome_value(f: &Fixture, action_id: Uuid) -> Result<f64, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, f64>(
        "SELECT observed_value FROM autopilot_outcomes
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await?)
}

/// A: nobody answered, the window closed, the measurement still resolves —
/// silence is a measured zero, and the outcome row carries it.
#[tokio::test]
async fn a_silence_resolves_to_a_measured_zero() -> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let target = outreach_target(&f, "quiet@example.com").await?;
    let sent_at = f.now - time::Duration::days(10);
    let action_id = insert_dispatch(&f, sent_at).await?;
    queue_reply_measurement(
        &f,
        action_id,
        target,
        AutopilotMeasurementKind::OutreachReply7d,
        sent_at,
    )
    .await?;
    // An outbound row exists — the letter really was sent — and no inbound
    // row followed it.
    interaction(
        &f,
        "outreach_interactions",
        target,
        "outbound",
        "letter",
        sent_at,
    )
    .await?;

    let (observed, measurement) = run_due_measurement(&f, f.now).await?;
    assert_eq!(
        observed, 0.0,
        "no reply in the window must resolve, not wait"
    );
    assert_eq!(
        outcome_value(&f, action_id).await?,
        0.0,
        "the outcome the learner reads is a measured zero, not a gap"
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM autopilot_measurements WHERE id = $1")
            .bind(measurement.id.into_uuid())
            .fetch_one(&f.pool)
            .await?;
    assert_eq!(status, "succeeded");
    Ok(())
}

/// B: a reply inside the window resolves to 1.0.
#[tokio::test]
async fn b_a_reply_inside_the_window_resolves_to_one() -> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let target = outreach_target(&f, "answered@example.com").await?;
    let sent_at = f.now - time::Duration::days(10);
    let action_id = insert_dispatch(&f, sent_at).await?;
    queue_reply_measurement(
        &f,
        action_id,
        target,
        AutopilotMeasurementKind::OutreachReply7d,
        sent_at,
    )
    .await?;
    interaction(
        &f,
        "outreach_interactions",
        target,
        "outbound",
        "letter",
        sent_at,
    )
    .await?;
    interaction(
        &f,
        "outreach_interactions",
        target,
        "inbound",
        "the-reply",
        sent_at + time::Duration::days(2),
    )
    .await?;

    let (observed, _) = run_due_measurement(&f, f.now).await?;
    assert_eq!(observed, 1.0);
    assert_eq!(outcome_value(&f, action_id).await?, 1.0);
    Ok(())
}

/// C: a reply that arrives after the seven-day window is still "no reply" —
/// the measurement answers the question it was scheduled to ask.
#[tokio::test]
async fn c_a_reply_after_the_window_is_still_silence() -> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let target = outreach_target(&f, "late@example.com").await?;
    let sent_at = f.now - time::Duration::days(20);
    let action_id = insert_dispatch(&f, sent_at).await?;
    queue_reply_measurement(
        &f,
        action_id,
        target,
        AutopilotMeasurementKind::OutreachReply7d,
        sent_at,
    )
    .await?;
    interaction(
        &f,
        "outreach_interactions",
        target,
        "outbound",
        "letter",
        sent_at,
    )
    .await?;
    // Day 9 — outside the 7-day window the measurement covers.
    interaction(
        &f,
        "outreach_interactions",
        target,
        "inbound",
        "the-late-reply",
        sent_at + time::Duration::days(9),
    )
    .await?;

    let (observed, _) = run_due_measurement(&f, f.now).await?;
    assert_eq!(
        observed, 0.0,
        "a reply outside the window is not this measurement's answer"
    );
    Ok(())
}

/// D: two booking letters to the same promoter, one reply. The reply belongs
/// to whichever letter was newest when it arrived — the earlier letter's
/// measurement resolves to 0.0, the later one's to 1.0, through the worker's
/// own observation path.
#[tokio::test]
async fn d_the_reply_belongs_to_the_letter_that_preceded_it()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let target = booking_target(&f, "promoter@example.com").await?;
    let first_letter = f.now - time::Duration::days(10);
    let second_letter = first_letter + time::Duration::days(2);
    let reply_at = second_letter + time::Duration::hours(6);

    let first_action = insert_dispatch(&f, first_letter).await?;
    let second_action = insert_dispatch(&f, second_letter).await?;
    queue_reply_measurement(
        &f,
        first_action,
        target,
        AutopilotMeasurementKind::BookingReply7d,
        first_letter,
    )
    .await?;
    queue_reply_measurement(
        &f,
        second_action,
        target,
        AutopilotMeasurementKind::BookingReply7d,
        second_letter,
    )
    .await?;
    for (direction, key, at) in [
        ("outbound", "letter-one", first_letter),
        ("outbound", "letter-two", second_letter),
        ("inbound", "the-reply", reply_at),
    ] {
        interaction(&f, "booking_interactions", target, direction, key, at).await?;
    }

    // Both measurements are due; claim order is due_at then id, so the first
    // letter resolves first — the order the worker would take them in.
    let (first_observed, first_measurement) = run_due_measurement(&f, f.now).await?;
    let (second_observed, _) = run_due_measurement(&f, f.now).await?;
    let (earlier, later) = if first_measurement.action_id.into_uuid() == first_action {
        (first_observed, second_observed)
    } else {
        (second_observed, first_observed)
    };
    assert_eq!(
        earlier, 0.0,
        "the earlier letter must not claim a reply a newer letter earned"
    );
    assert_eq!(
        later, 1.0,
        "the letter the promoter actually answered owns the reply"
    );
    assert_eq!(outcome_value(&f, second_action).await?, 1.0);
    assert_eq!(outcome_value(&f, first_action).await?, 0.0);
    Ok(())
}
