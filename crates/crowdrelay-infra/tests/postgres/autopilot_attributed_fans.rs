//! Attributed fan outcomes across an action's lineage, against a real
//! Postgres. Borrows the measurement-spine fixtures.
//!
//! The measured action is usually not the one whose post carried the link: an
//! `agent.run.request` is measured, and the post belongs to the
//! `community.engage.request` its outcome created. These tests pin the two
//! lineage links (`trace_id`, and agent outcome → task `metadata.action_id`),
//! the window opening at the first live tracked post, and the durable
//! filter.

use crowdrelay_application::autopilot::{
    AutopilotMeasurementKind, AutopilotMeasurementRepository, ClaimedAutopilotMeasurement,
};
use time::OffsetDateTime;

use crate::autopilot_measurement_spine::{Fixture, insert_dispatch, queue_measurement, setup};

/// A second action in the same workspace, to stand in for the post the
/// measured action's work produced.
async fn child_action(f: &Fixture, label: &str) -> uuid::Uuid {
    let child = insert_dispatch(f, label, f.now).await;
    sqlx::query(
        "UPDATE autopilot_actions SET action_kind = 'community.engage.request' WHERE id = $1",
    )
    .bind(child)
    .execute(&f.pool)
    .await
    .expect("child kind");
    child
}

pub(crate) async fn live_post(
    f: &Fixture,
    action_id: uuid::Uuid,
    slug: &str,
    posted_at: OffsetDateTime,
) {
    sqlx::query(
        r#"INSERT INTO community_posts
           (workspace_id, action_id, subreddit, title, body, status, posted_at, smart_link)
           VALUES ($1,$2,'r/lineage','t','b','posted',$3,$4)"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(posted_at)
    .bind(format!("/l/{slug}"))
    .execute(&f.pool)
    .await
    .expect("post");
}

/// A fan converted through `action_id`'s link at `at`, created at `created`.
pub(crate) async fn converted_fan(
    f: &Fixture,
    action_id: uuid::Uuid,
    at: OffsetDateTime,
    created: OffsetDateTime,
    status: &str,
) -> uuid::Uuid {
    let fan_id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status, created_at) \
         VALUES ($1,$2,$3,$4,$5)",
    )
    .bind(fan_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("{fan_id}@example.test"))
    .bind(status)
    .bind(created)
    .execute(&f.pool)
    .await
    .expect("fan");
    sqlx::query(
        r#"INSERT INTO fan_provenance_events
           (workspace_id, fan_id, event_kind, channel, action_id,
            attribution_method, attribution_confidence, occurred_at)
           VALUES ($1,$2,'conversion','reddit',$3,'last_tracked_click',1.0,$4)"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(fan_id)
    .bind(action_id)
    .bind(at)
    .execute(&f.pool)
    .await
    .expect("conversion");
    fan_id
}

pub(crate) async fn marketing_consent(
    f: &Fixture,
    fan_id: uuid::Uuid,
    granted: bool,
    recorded_at: OffsetDateTime,
) {
    sqlx::query(
        "INSERT INTO fan_consents
           (workspace_id, fan_id, purpose, granted, policy_version, source, recorded_at)
         VALUES ($1,$2,'marketing',$3,'v1','retention-test',$4)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(fan_id)
    .bind(granted)
    .bind(recorded_at)
    .execute(&f.pool)
    .await
    .expect("consent");
}

pub(crate) async fn meaningful_session(
    f: &Fixture,
    fan_id: uuid::Uuid,
    last_seen_at: OffsetDateTime,
) {
    let mut hash = fan_id.as_bytes().to_vec();
    hash.extend_from_slice(fan_id.as_bytes());
    sqlx::query(
        "INSERT INTO fan_sessions
           (workspace_id, fan_id, session_token_hash, created_at, last_seen_at, expires_at)
         VALUES ($1,$2,$3,$4,$5,$6)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(fan_id)
    .bind(hash)
    .bind(last_seen_at - time::Duration::days(1))
    .bind(last_seen_at)
    .bind(f.now + time::Duration::days(30))
    .execute(&f.pool)
    .await
    .expect("meaningful session");
}

async fn observe(f: &Fixture, measurement: &ClaimedAutopilotMeasurement) -> f64 {
    f.repository
        .observe_measurement(f.workspace_id, measurement, f.now)
        .await
        .expect("an instrumented lineage is measurable")
}

/// A child sharing the measured action's trace carries its fans up.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_child_on_the_same_trace_carries_its_fans_to_the_parent() {
    let f = setup().await.expect("fixture");
    let started = f.now - time::Duration::days(14);
    let root = insert_dispatch(&f, "lineage:trace", started).await;
    let child = child_action(&f, "lineage:trace-child").await;
    sqlx::query("UPDATE autopilot_actions SET trace_id = $3 WHERE id IN ($1, $2)")
        .bind(root)
        .bind(child)
        .bind(uuid::Uuid::now_v7())
        .execute(&f.pool)
        .await
        .expect("shared trace");
    live_post(&f, child, "trace-child", started + time::Duration::hours(1)).await;
    let converted = started + time::Duration::days(1);
    converted_fan(&f, child, converted, converted, "active").await;

    let measurement = queue_measurement(
        &f,
        root,
        AutopilotMeasurementKind::AgentRunFanGrowth14d,
        14.8,
        started,
    )
    .await;
    assert!(
        (observe(&f, &measurement).await - 1.0).abs() < f64::EPSILON,
        "the child's traced fan is the parent's outcome"
    );
}

/// A child created from an agent outcome whose task names the measured action
/// carries its fans up, even on a different trace.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_child_created_from_the_runs_outcome_carries_its_fans_to_the_run() {
    let f = setup().await.expect("fixture");
    let started = f.now - time::Duration::days(14);
    let run = insert_dispatch(&f, "lineage:outcome", started).await;
    let child = child_action(&f, "lineage:outcome-child").await;
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS agent_service_tasks (
               id uuid PRIMARY KEY,
               workspace_id uuid NOT NULL,
               template_id text NOT NULL,
               model_id text NOT NULL,
               prompt text NOT NULL,
               status text NOT NULL DEFAULT 'queued',
               tier text NOT NULL DEFAULT 'basic',
               metadata jsonb NOT NULL DEFAULT '{}'::jsonb,
               created_at timestamptz NOT NULL DEFAULT now()
           )"#,
    )
    .execute(&f.pool)
    .await
    .expect("foreign task table");
    let task_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO agent_service_tasks
               (id, workspace_id, template_id, model_id, prompt, status, metadata)
           VALUES ($1,$2,'community-engager','auto','probe','completed',$3)"#,
    )
    .bind(task_id)
    .bind(f.workspace_id.into_uuid())
    .bind(serde_json::json!({"action_id": run}))
    .execute(&f.pool)
    .await
    .expect("task");
    let outcome_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO agent_outcomes
               (id, workspace_id, task_id, result_id, kind, schema_version, payload,
                confidence_basis_points, idempotency_key, status, processed_action_id)
           VALUES ($1,$2,$3,$4,'social_post',1,'{}'::jsonb,7500,$5,'processed',$6)"#,
    )
    .bind(outcome_id)
    .bind(f.workspace_id.into_uuid())
    .bind(task_id)
    .bind(uuid::Uuid::now_v7())
    .bind(format!("lineage-{outcome_id}"))
    .bind(child)
    .execute(&f.pool)
    .await
    .expect("outcome");
    live_post(
        &f,
        child,
        "outcome-child",
        started + time::Duration::hours(2),
    )
    .await;
    let converted = started + time::Duration::days(2);
    converted_fan(&f, child, converted, converted, "active").await;

    let measurement = queue_measurement(
        &f,
        run,
        AutopilotMeasurementKind::IncrementalFanGrowth14d,
        0.8,
        started,
    )
    .await;
    assert!(
        (observe(&f, &measurement).await - 1.0).abs() < f64::EPSILON,
        "the outcome's action is the run's lineage"
    );
}

/// The window opens when the first tracked post went live. A draft published
/// ten days after the run finished still has its full three days.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_window_opens_when_the_tracked_post_went_live() {
    let f = setup().await.expect("fixture");
    let finished = f.now - time::Duration::days(12);
    let action = insert_dispatch(&f, "lineage:late", finished).await;
    let posted = finished + time::Duration::days(10);
    live_post(&f, action, "late", posted).await;
    let converted = posted + time::Duration::days(1);
    converted_fan(&f, action, converted, converted, "active").await;

    let measurement = queue_measurement(
        &f,
        action,
        AutopilotMeasurementKind::AgentRunFanGrowth3d,
        0.0,
        finished,
    )
    .await;
    assert!(
        (observe(&f, &measurement).await - 1.0).abs() < f64::EPSILON,
        "a conversion a day after a late post is inside that post's three days"
    );
}

/// Durable means meaningfully retained, not merely an account that survived.
///
/// The qualifying fan is active, currently consented and returned through a
/// first-party session after the thirty-day maturity boundary. Active-but-silent,
/// never-consented, consent-revoked and unsubscribed conversions must all stay
/// out of the North Star.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn durable_counts_only_meaningfully_retained_consented_fans() {
    let f = setup().await.expect("fixture");
    let finished = f.now - time::Duration::days(44);
    let action = insert_dispatch(&f, "lineage:durable", finished).await;
    live_post(&f, action, "durable", finished + time::Duration::hours(1)).await;
    let converted = finished + time::Duration::days(3);
    let returned_at = f.now - time::Duration::days(1);

    let retained = converted_fan(&f, action, converted, converted, "active").await;
    marketing_consent(&f, retained, true, converted).await;
    meaningful_session(&f, retained, returned_at).await;

    let silent = converted_fan(&f, action, converted, converted, "active").await;
    marketing_consent(&f, silent, true, converted).await;

    let no_consent = converted_fan(&f, action, converted, converted, "active").await;
    meaningful_session(&f, no_consent, returned_at).await;

    let revoked = converted_fan(&f, action, converted, converted, "active").await;
    marketing_consent(&f, revoked, true, converted).await;
    marketing_consent(&f, revoked, false, f.now - time::Duration::days(2)).await;
    meaningful_session(&f, revoked, returned_at).await;

    let unsubscribed = converted_fan(&f, action, converted, converted, "unsubscribed").await;
    marketing_consent(&f, unsubscribed, true, converted).await;
    meaningful_session(&f, unsubscribed, returned_at).await;

    let measurement = queue_measurement(
        &f,
        action,
        AutopilotMeasurementKind::DurableFanGrowth30d,
        0.5,
        finished,
    )
    .await;
    assert!(
        (observe(&f, &measurement).await - 1.0).abs() < f64::EPSILON,
        "only the fan with current consent and a meaningful post-D30 return is durable"
    );
}

/// A fan credited to another action, and a fan who arrived with no
/// provenance, are not this action's — the workspace-window count included
/// both.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn fans_credited_elsewhere_or_nowhere_are_not_counted() {
    let f = setup().await.expect("fixture");
    let finished = f.now - time::Duration::days(14);
    let action = insert_dispatch(&f, "lineage:mine", finished).await;
    let other = insert_dispatch(&f, "lineage:other", finished).await;
    live_post(&f, action, "mine", finished + time::Duration::hours(1)).await;
    live_post(&f, other, "other", finished + time::Duration::hours(1)).await;
    let converted = finished + time::Duration::days(1);
    converted_fan(&f, action, converted, converted, "active").await;
    converted_fan(&f, other, converted, converted, "active").await;
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status, created_at) \
         VALUES ($1,$2,'no-provenance@example.test','active',$3)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(f.workspace_id.into_uuid())
    .bind(converted)
    .execute(&f.pool)
    .await
    .expect("bare fan");

    let measurement = queue_measurement(
        &f,
        action,
        AutopilotMeasurementKind::IncrementalFanGrowth14d,
        0.0,
        finished,
    )
    .await;
    assert!(
        (observe(&f, &measurement).await - 1.0).abs() < f64::EPSILON,
        "only the fan traced to this action counts"
    );
}
