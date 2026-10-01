//! Publication maturity is queue scheduling, not a failure to retry three times.

use crate::autopilot_attributed_fans::{converted_fan, live_post};
use crate::autopilot_measurement_spine::{insert_dispatch, queue_measurement, setup};
use crowdrelay_application::autopilot::{AutopilotMeasurementKind, AutopilotMeasurementRepository};

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn immature_fan_windows_do_not_spend_attempts_or_block_ready_work() {
    let f = setup().await.expect("fixture");
    let now = f.now.replace_nanosecond(0).expect("whole second");
    let finished = now - time::Duration::days(60);
    let action = insert_dispatch(&f, "window:late", finished).await;
    let posted = now - time::Duration::days(1);
    live_post(&f, action, "window-late", posted).await;
    let mut held = Vec::new();
    for (kind, days) in [
        (AutopilotMeasurementKind::AgentRunFanGrowth3d, 3),
        (AutopilotMeasurementKind::IncrementalFanGrowth3d, 3),
        (AutopilotMeasurementKind::AgentRunFanGrowth14d, 14),
        (AutopilotMeasurementKind::IncrementalFanGrowth14d, 14),
        (AutopilotMeasurementKind::DurableFanGrowth30d, 44),
    ] {
        held.push((
            queue_measurement(&f, action, kind, 0.0, finished).await,
            days,
        ));
    }
    let ready = queue_measurement(
        &f,
        action,
        AutopilotMeasurementKind::ArtifactOutcome7d,
        0.0,
        finished,
    )
    .await;
    let claimed = f
        .repository
        .claim_due_measurements(f.workspace_id, 100, now)
        .await
        .expect("claim");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, ready.id);
    for (measurement, days) in held {
        let row: (String, i32, time::OffsetDateTime, time::OffsetDateTime) = sqlx::query_as(
            "SELECT status, attempt_count, due_at, available_at FROM autopilot_measurements WHERE id=$1",
        ).bind(measurement.id.into_uuid()).fetch_one(&f.pool).await.expect("deferred");
        assert_eq!(row.0, "pending");
        assert_eq!(row.1, 0);
        assert_eq!(row.2, posted + time::Duration::days(days));
        assert_eq!(row.3, row.2);
    }
    assert!(
        f.repository
            .claim_due_measurements(f.workspace_id, 100, now + time::Duration::minutes(10))
            .await
            .expect("repeat claim")
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn durable_fans_wait_for_the_last_arrival_in_a_late_published_cohort() {
    let f = setup().await.expect("fixture");
    let now = f.now.replace_nanosecond(0).expect("whole second");
    let finished = now - time::Duration::days(60);
    let posted = now - time::Duration::days(40);
    let action = insert_dispatch(&f, "window:durable", finished).await;
    live_post(&f, action, "window-durable", posted).await;
    let first = posted + time::Duration::days(1);
    let last = posted + time::Duration::days(13);
    converted_fan(&f, action, first, first, "active").await;
    converted_fan(&f, action, last, last, "active").await;
    queue_measurement(
        &f,
        action,
        AutopilotMeasurementKind::DurableFanGrowth30d,
        0.0,
        finished,
    )
    .await;
    assert!(
        f.repository
            .claim_due_measurements(f.workspace_id, 100, now)
            .await
            .expect("immature cohort")
            .is_empty()
    );
    let at = posted + time::Duration::days(44);
    let claimed = f
        .repository
        .claim_due_measurements(f.workspace_id, 100, at)
        .await
        .expect("mature cohort");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].attempt_number, 1);
    assert_eq!(
        f.repository
            .observe_measurement(f.workspace_id, &claimed[0], at)
            .await
            .expect("observe full cohort"),
        2.0
    );
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_future_conversion_does_not_teach_an_acquisition_that_has_not_happened() {
    let f = setup().await.expect("fixture");
    let finished = f.now - time::Duration::days(2);
    let action = insert_dispatch(&f, "window:future", finished).await;
    live_post(&f, action, "window-future", finished).await;
    converted_fan(
        &f,
        action,
        f.now - time::Duration::hours(1),
        f.now - time::Duration::hours(1),
        "active",
    )
    .await;
    converted_fan(
        &f,
        action,
        f.now + time::Duration::hours(1),
        f.now + time::Duration::hours(1),
        "active",
    )
    .await;
    let measurement = queue_measurement(
        &f,
        action,
        AutopilotMeasurementKind::IncrementalFanGrowth3d,
        0.0,
        finished,
    )
    .await;
    assert_eq!(
        f.repository
            .observe_measurement(f.workspace_id, &measurement, f.now)
            .await
            .expect("as-of acquisition"),
        1.0
    );
}
