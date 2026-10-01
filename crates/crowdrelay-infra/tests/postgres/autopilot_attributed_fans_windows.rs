//! Publication maturity is queue scheduling, not a failure to retry three times.

use crate::autopilot_attributed_fans::{
    converted_fan, live_post, marketing_consent, meaningful_session,
};
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
    let observed_at = posted + time::Duration::days(44);
    // Durable is meaningfully-retained now: consent plus a meaningful action
    // inside each fan's own trailing-30d window past their +30d maturity.
    for (acquired_at, session_at) in [
        (first, posted + time::Duration::days(40)),
        (
            last,
            posted + time::Duration::days(43) + time::Duration::hours(12),
        ),
    ] {
        let fan = converted_fan(&f, action, acquired_at, acquired_at, "active").await;
        marketing_consent(&f, fan, true, acquired_at).await;
        meaningful_session(&f, fan, session_at).await;
    }
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
    let claimed = f
        .repository
        .claim_due_measurements(f.workspace_id, 100, observed_at)
        .await
        .expect("mature cohort");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].attempt_number, 1);
    assert_eq!(
        f.repository
            .observe_measurement(f.workspace_id, &claimed[0], observed_at)
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

/// The content 7d kinds share the same publication clock: a measurement the
/// dispatcher scheduled at action-finish waits out the real `posted_at`
/// window rather than reading a four-day partial as a finished seven.
/// Deferral spends no attempt and does not block a genuinely due sibling.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn content_measurements_wait_for_the_publication_window() {
    let f = setup().await.expect("fixture");
    let now = f.now.replace_nanosecond(0).expect("whole second");
    let finished = now - time::Duration::days(60);
    let action = insert_dispatch(&f, "window:content", finished).await;
    // Published only a day ago: the action's clock says due, the
    // publication's says six days remain.
    let posted = now - time::Duration::days(1);
    live_post(&f, action, "window-content", posted).await;

    let mut held = Vec::new();
    for kind in [
        AutopilotMeasurementKind::ContentLinkClicks7d,
        AutopilotMeasurementKind::ContentFanAcquisition7d,
    ] {
        held.push(queue_measurement(&f, action, kind, 0.0, finished).await);
    }

    let claimed = f
        .repository
        .claim_due_measurements(f.workspace_id, 100, now)
        .await
        .expect("claim");
    assert!(claimed.is_empty(), "an immature window is not claimed");
    for measurement in &held {
        let row: (String, i32, String, time::OffsetDateTime) = sqlx::query_as(
            "SELECT status, attempt_count, last_error_kind, due_at \
             FROM autopilot_measurements WHERE id=$1",
        )
        .bind(measurement.id.into_uuid())
        .fetch_one(&f.pool)
        .await
        .expect("deferred");
        assert_eq!(row.0, "pending");
        assert_eq!(row.1, 0, "waiting on publication costs no attempt");
        assert_eq!(row.2, "awaiting_publication_window");
        assert_eq!(row.3, posted + time::Duration::days(7));
    }

    // At window close the measurements claim and observe on the
    // publication clock — one attempt, honest zero.
    let mature = posted + time::Duration::days(8);
    let claimed = f
        .repository
        .claim_due_measurements(f.workspace_id, 100, mature)
        .await
        .expect("mature claim");
    assert_eq!(claimed.len(), 2);
    assert!(claimed.iter().all(|row| row.attempt_number == 1));
}

/// A content measurement whose posts can still publish waits on a short
/// re-check clock instead of resolving `no_tracked_link` while the post is
/// merely queued — and when every post has died, it claims so the observer
/// answers `no_tracked_link` honestly.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn content_measurements_await_a_post_that_can_still_publish() {
    let f = setup().await.expect("fixture");
    let now = f.now.replace_nanosecond(0).expect("whole second");
    let finished = now - time::Duration::days(60);
    let action = insert_dispatch(&f, "window:unpublished", finished).await;

    // A pending manual post — nothing live yet, but it can still go out.
    sqlx::query(
        r#"INSERT INTO community_posts
           (workspace_id, action_id, subreddit, title, body, status, smart_link)
           VALUES ($1,$2,'r/lineage','t','b','awaiting_manual_post','/l/window-pending')"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action)
    .execute(&f.pool)
    .await
    .expect("pending post");
    let held = queue_measurement(
        &f,
        action,
        AutopilotMeasurementKind::ContentFanAcquisition7d,
        0.0,
        finished,
    )
    .await;
    let claimed = f
        .repository
        .claim_due_measurements(f.workspace_id, 100, now)
        .await
        .expect("claim");
    assert!(claimed.is_empty());
    let row: (String, String, time::OffsetDateTime) = sqlx::query_as(
        "SELECT status, last_error_kind, due_at FROM autopilot_measurements WHERE id=$1",
    )
    .bind(held.id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("deferred");
    assert_eq!(row.0, "pending");
    assert_eq!(row.1, "awaiting_publication");
    assert_eq!(row.2, now + time::Duration::hours(6));

    // The manual post dies instead — nothing left can publish, so the
    // measurement must claim and let the observer say no_tracked_link.
    sqlx::query(
        "UPDATE community_posts SET status = 'failed' \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action)
    .execute(&f.pool)
    .await
    .expect("kill post");
    let claimed = f
        .repository
        .claim_due_measurements(f.workspace_id, 100, now + time::Duration::hours(7))
        .await
        .expect("claim after post death");
    assert_eq!(claimed.len(), 1);
}
