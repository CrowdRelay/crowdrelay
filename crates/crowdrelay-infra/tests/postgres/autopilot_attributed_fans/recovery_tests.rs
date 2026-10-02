use super::*;
use crowdrelay_application::RepositoryError;
use crowdrelay_application::autopilot::{HarmObservation, assess_measurement_effect};

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn late_publication_reopens_only_never_observed_failures() {
    let f = setup().await.expect("fixture");
    let finished = f.now - time::Duration::days(20);
    let action = insert_dispatch(&f, "late-recovery", finished).await;
    let m = queue_measurement(
        &f,
        action,
        AutopilotMeasurementKind::AgentRunFanGrowth3d,
        0.0,
        finished,
    )
    .await;
    sqlx::query("UPDATE autopilot_measurements SET status='failed',attempt_count=1,finished_at=$2,last_error_kind='no_tracked_link' WHERE id=$1")
        .bind(m.id.into_uuid()).bind(f.now-time::Duration::days(2)).execute(&f.pool).await.expect("failure");
    // Postgres keeps microseconds; the due_at assert below compares against
    // this value, so it must be whole-second to round-trip.
    let posted = (f.now - time::Duration::days(1))
        .replace_nanosecond(0)
        .expect("zero nanoseconds is valid");
    live_post(&f, action, "late-recovery", posted).await;
    assert!(
        f.repository
            .claim_due_measurements(f.workspace_id, 100, f.now)
            .await
            .expect("recovery")
            .is_empty()
    );
    let state: (String, i32, OffsetDateTime) = sqlx::query_as(
        "SELECT status,attempt_count,due_at FROM autopilot_measurements WHERE id=$1",
    )
    .bind(m.id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("state");
    assert_eq!(
        state,
        ("pending".into(), 0, posted + time::Duration::days(3))
    );
    let claims = f
        .repository
        .claim_due_measurements(f.workspace_id, 100, posted + time::Duration::days(3))
        .await
        .expect("mature claim");
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].attempt_number, 1);
    assert!(
        f.repository
            .claim_due_measurements(f.workspace_id, 100, posted + time::Duration::days(3))
            .await
            .expect("repeat")
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn stale_writeback_cannot_complete_or_fail_a_newer_attempt() {
    let f = setup().await.expect("fixture");
    let finished = f.now - time::Duration::days(20);
    let action = insert_dispatch(&f, "claim-fence", finished).await;
    let old = queue_measurement(
        &f,
        action,
        AutopilotMeasurementKind::ArtifactOutcome7d,
        0.0,
        finished,
    )
    .await;
    sqlx::query("UPDATE autopilot_measurements SET status='processing',attempt_count=2,started_at=$2 WHERE id=$1")
        .bind(old.id.into_uuid()).bind(f.now).execute(&f.pool).await.expect("new owner");
    let effect = assess_measurement_effect(&old, 1.0, &HarmObservation::default()).expect("effect");
    assert_eq!(
        f.repository
            .complete_measurement(f.workspace_id, &old, 1.0, effect, None, f.now)
            .await,
        Err(RepositoryError::Conflict)
    );
    f.repository
        .fail_measurement(f.workspace_id, &old, "stale_failure", false, None, f.now)
        .await
        .expect("stale no-op");
    let state: (String, i32, Option<String>) = sqlx::query_as(
        "SELECT status,attempt_count,last_error_kind FROM autopilot_measurements WHERE id=$1",
    )
    .bind(old.id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("state");
    assert_eq!(state, ("processing".into(), 2, None));
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM autopilot_outcomes WHERE measurement_id=$1")
            .bind(old.id.into_uuid())
            .fetch_one(&f.pool)
            .await
            .expect("outcomes");
    assert_eq!(count, 0);
    let current = ClaimedAutopilotMeasurement {
        attempt_number: 2,
        ..old
    };
    let effect =
        assess_measurement_effect(&current, 1.0, &HarmObservation::default()).expect("effect");
    f.repository
        .complete_measurement(f.workspace_id, &current, 1.0, effect, None, f.now)
        .await
        .expect("current completion");
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn learned_evidence_and_non_publication_failures_are_not_resurrected() {
    let f = setup().await.expect("fixture");
    for (label, error, observed) in [
        ("learned", "no_tracked_link", Some(1.0)),
        ("permanent", "unsupported_measurement_kind", None),
    ] {
        let action = insert_dispatch(&f, label, f.now - time::Duration::days(30)).await;
        let m = queue_measurement(
            &f,
            action,
            AutopilotMeasurementKind::AgentRunFanGrowth3d,
            0.0,
            f.now - time::Duration::days(30),
        )
        .await;
        sqlx::query("UPDATE autopilot_measurements SET status='failed',attempt_count=1,finished_at=$2,last_error_kind=$3 WHERE id=$1")
            .bind(m.id.into_uuid()).bind(f.now-time::Duration::days(10)).bind(error).execute(&f.pool).await.expect("failure");
        sqlx::query("UPDATE growth_evidence SET observed_fans=$2 WHERE action_id=$1")
            .bind(action)
            .bind(observed)
            .execute(&f.pool)
            .await
            .expect("evidence");
        live_post(
            &f,
            action,
            &format!("recovery-{label}"),
            f.now - time::Duration::days(1),
        )
        .await;
    }
    assert!(
        f.repository
            .claim_due_measurements(f.workspace_id, 100, f.now)
            .await
            .expect("claim")
            .is_empty()
    );
}
