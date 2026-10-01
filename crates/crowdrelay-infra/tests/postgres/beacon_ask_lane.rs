//! The beacon human-send lane, against real Postgres.
//!
//! `beacon.outreach` and `beacon.invite_batch` sit in PENDING_ROUTE — no n8n
//! route consumes them, so outward asks die as `awaiting_executor` parks or
//! as emissions whose deliveries never happened. The lane hands each ask to
//! the operator instead: `queue` lists it, `prepare` re-verifies the partner
//! under the executor's own guards and mints the letter's tracked links, and
//! `sent` files the ordinary terminal receipt under `operator-console`.
//!
//! What only real rows can prove:
//!
//! - A parked ask surfaces in the queue, prepares into a letter, and its
//!   send resolves the action `succeeded` with a report row — the same
//!   evidence an executor receipt leaves, minus the provider reference.
//! - `beacon_campaigns.followup_count` feeds the decision keys that decide
//!   whether the next proposal fires, so a re-prepare must rebuild the
//!   letter without touching the counter again.
//! - A stale beacon version, an unprepared send, and a second `sent` all
//!   refuse or replay — the lane tolerates retries and nothing else.
//! - An action emitted-but-never-delivered (and the `unknown` state the
//!   gap sweep gives it a day later) resurfaces as `unreached` and the
//!   operator's receipt is the confirming one.

use std::time::Duration;

use crate::common;
use crowdrelay_application::RepositoryError;
use crowdrelay_application::autopilot::{
    AutopilotRuntimeRepository, ExecutorReportStatus, RecordExecutionReport,
};
use crowdrelay_domain::{AutopilotActionId, WorkspaceId};
use crowdrelay_infra::{
    autopilot::{
        OPERATOR_EXECUTOR_ID, PostgresAutopilotRepository, beacon_ask_send_claim,
        list_beacon_ask_queue, prepare_beacon_ask,
    },
    config::DatabaseConfig,
};
use serde_json::json;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

struct Fixture {
    pool: PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    city_id: Uuid,
}

async fn setup() -> Result<Fixture, Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("beacon-lane-{}", workspace_id.into_uuid().simple()))
        .bind("Beacon Lane Tests")
        .execute(&pool)
        .await?;
    let suffix = &workspace_id.into_uuid().simple().to_string()[20..];
    let city_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO cities (name, country_code, slug) VALUES ($1, 'PL', $2) RETURNING id",
    )
    .bind(format!("Zielona {suffix}"))
    .bind(format!("zielona-{suffix}"))
    .fetch_one(&pool)
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
        city_id,
    })
}

/// A verified, accepting beacon plus a published show — the shape the
/// decision evaluator gated on when it wrote the ask.
async fn pair(
    f: &Fixture,
    starts_in_days: i64,
) -> Result<(Uuid, i64, Uuid), Box<dyn std::error::Error>> {
    let suffix = &Uuid::now_v7().simple().to_string()[20..];
    let event_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO events
            (workspace_id, slug, title, status, starts_at, timezone, city_id, published_at)
        VALUES ($1, $2, 'Lane night', 'published', $3, 'Europe/Warsaw', $4, now())
        RETURNING id
        "#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(format!("lane-night-{suffix}"))
    .bind(OffsetDateTime::now_utc() + time::Duration::days(starts_in_days))
    .bind(f.city_id)
    .fetch_one(&f.pool)
    .await?;
    let (beacon_id, beacon_version) = sqlx::query_as::<_, (Uuid, i64)>(
        r#"
        INSERT INTO beacons
            (workspace_id, city_id, beacon_kind, display_name, contact_email,
             active, verified, accepts_outreach, relationship_score)
        VALUES ($1, $2, 'local_press', $3, $4, true, true, true, 50)
        RETURNING id, version
        "#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(f.city_id)
    .bind(format!("Gazeta {suffix}"))
    .bind(format!("gazeta-{suffix}@example.test"))
    .fetch_one(&f.pool)
    .await?;
    Ok((beacon_id, beacon_version, event_id))
}

async fn decision(f: &Fixture, beacon_id: Uuid) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
           VALUES ($1,$2,$3,'beacon','beacon',$4,'amplify_local_signal',9000,
                   'require_approval','seeded ask','{}','{}','{}',now(),$1)"#,
    )
    .bind(decision_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("decision-{decision_id}"))
    .bind(beacon_id)
    .execute(&f.pool)
    .await?;
    Ok(decision_id)
}

/// The ask exactly as the approval path leaves it: approved, dispatched, and
/// parked on the capability nobody advertises.
async fn parked_ask(
    f: &Fixture,
    beacon_id: Uuid,
    beacon_version: i64,
    event_id: Uuid,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, action_class,
            last_error_kind)
           VALUES ($1,$2,$3,'beacon','beacon.outreach.request','beacon',$4,$5,$6,
                   'queued','third_party','awaiting_executor')"#,
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .bind(decision(f, beacon_id).await?)
    .bind(beacon_id)
    .bind(format!("action-{action_id}"))
    .bind(json!({
        "kind": "request_beacon_outreach",
        "beacon_id": beacon_id,
        "event_id": event_id,
        "beacon_version": beacon_version,
        "phase": "initial",
        "template_key": "beacon.local_story.v1",
    }))
    .execute(&f.pool)
    .await?;
    Ok(action_id)
}

/// The send half of the lane exactly as the API composes it: claim token
/// from the lane, then the ordinary terminal receipt.
async fn operator_send(f: &Fixture, action_id: Uuid) -> Result<bool, Box<dyn std::error::Error>> {
    let claim = beacon_ask_send_claim(&f.pool, f.workspace_id, action_id).await?;
    let mutation = f
        .repository
        .record_execution_report(
            f.workspace_id,
            RecordExecutionReport {
                action_id: AutopilotActionId::from_uuid(action_id),
                receipt_key: format!("operator-sent:{action_id}"),
                executor_id: OPERATOR_EXECUTOR_ID.to_owned(),
                status: ExecutorReportStatus::Succeeded,
                claim_token: Some(claim.claim_token),
                provider_reference: None,
                error_kind: None,
                metadata: json!({"transport": "operator"}),
                occurred_at: OffsetDateTime::now_utc(),
            },
        )
        .await?;
    Ok(mutation.replayed)
}

async fn action_status(f: &Fixture, action_id: Uuid) -> Result<String, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT status FROM autopilot_actions WHERE workspace_id = $1 AND id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await?)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn parked_ask_prepares_sends_and_completes() -> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let (beacon_id, beacon_version, event_id) = pair(&f, 14).await?;
    let action_id = parked_ask(&f, beacon_id, beacon_version, event_id).await?;

    let queue = list_beacon_ask_queue(&f.pool, f.workspace_id).await?;
    let item = queue
        .iter()
        .find(|item| item.action_id == action_id)
        .expect("parked ask must surface in the queue");
    assert_eq!(
        item.lane_state,
        crowdrelay_infra::autopilot::BeaconAskLaneState::Parked
    );
    assert_eq!(item.kind, "outreach");

    let prepared = prepare_beacon_ask(&f.pool, f.workspace_id, action_id).await?;
    assert!(prepared.to.ends_with("@example.test"));
    assert!(prepared.body.contains("Lane night"));

    assert_eq!(action_status(&f, action_id).await?, "processing");
    let claim_status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM autopilot_execution_claims \
         WHERE workspace_id=$1 AND action_id=$2 AND executor_id=$3",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(OPERATOR_EXECUTOR_ID)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(claim_status, "claimed");

    // The campaign touch happened once — the send is committed now.
    let (campaign_status, followups) = sqlx::query_as::<_, (String, i32)>(
        "SELECT status, followup_count FROM beacon_campaigns \
         WHERE workspace_id=$1 AND beacon_id=$2 AND event_id=$3",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(beacon_id)
    .bind(event_id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(campaign_status, "contacted");
    assert_eq!(followups, 1);

    // The report path needs an emission row even though nothing was emitted.
    let marker = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM autopilot_action_emissions \
         WHERE workspace_id=$1 AND action_id=$2 AND outbox_event_id IS NULL)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await?;
    assert!(marker);

    assert!(!operator_send(&f, action_id).await?);
    assert_eq!(action_status(&f, action_id).await?, "succeeded");

    let report = sqlx::query_scalar::<_, Option<String>>(
        "SELECT provider_reference FROM autopilot_execution_reports \
         WHERE workspace_id=$1 AND action_id=$2 AND executor_id=$3 AND status='succeeded'",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(OPERATOR_EXECUTOR_ID)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(
        report, None,
        "a hand send is believed, not provider-confirmed"
    );

    // The queue no longer holds it.
    let queue = list_beacon_ask_queue(&f.pool, f.workspace_id).await?;
    assert!(queue.iter().all(|item| item.action_id != action_id));
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn reprepare_rebuilds_the_letter_without_a_second_touch()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let (beacon_id, beacon_version, event_id) = pair(&f, 14).await?;
    let action_id = parked_ask(&f, beacon_id, beacon_version, event_id).await?;

    let first = prepare_beacon_ask(&f.pool, f.workspace_id, action_id).await?;
    let second = prepare_beacon_ask(&f.pool, f.workspace_id, action_id).await?;
    assert_eq!(first.body, second.body);

    // followup_count feeds the next cycle's decision keys — a rebuild must
    // not count as another contact. The first prepare counts one touch.
    let followups = sqlx::query_scalar::<_, i32>(
        "SELECT followup_count FROM beacon_campaigns \
         WHERE workspace_id=$1 AND beacon_id=$2 AND event_id=$3",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(beacon_id)
    .bind(event_id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(followups, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn stale_beacon_version_refuses() -> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let (beacon_id, beacon_version, event_id) = pair(&f, 14).await?;
    let action_id = parked_ask(&f, beacon_id, beacon_version, event_id).await?;

    // The partner's record moved since approval — the send must refuse.
    sqlx::query("UPDATE beacons SET version = version + 1 WHERE id = $1")
        .bind(beacon_id)
        .execute(&f.pool)
        .await?;
    match prepare_beacon_ask(&f.pool, f.workspace_id, action_id).await {
        Err(RepositoryError::Conflict) => {}
        other => panic!("stale beacon version must conflict, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn sent_without_prepare_and_duplicate_send() -> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let (beacon_id, beacon_version, event_id) = pair(&f, 14).await?;
    let action_id = parked_ask(&f, beacon_id, beacon_version, event_id).await?;

    // No operator claim exists — `sent` refuses.
    match beacon_ask_send_claim(&f.pool, f.workspace_id, action_id).await {
        Err(RepositoryError::Conflict) => {}
        Err(error) => panic!("send without prepare must conflict, got {error:?}"),
        Ok(_) => panic!("send without prepare must conflict, got a claim"),
    }

    prepare_beacon_ask(&f.pool, f.workspace_id, action_id).await?;
    assert!(!operator_send(&f, action_id).await?);
    // The retried `sent` dedupes on the receipt key — replayed, not a second send.
    assert!(operator_send(&f, action_id).await?);

    let reports = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM autopilot_execution_reports \
         WHERE workspace_id=$1 AND action_id=$2 AND executor_id=$3",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(OPERATOR_EXECUTOR_ID)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(reports, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn unreached_action_resurfaces_and_completes() -> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let (beacon_id, beacon_version, event_id) = pair(&f, 14).await?;
    let action_id = parked_ask(&f, beacon_id, beacon_version, event_id).await?;

    // The executor-less dispatch: claimed, marked succeeded, emitted — and
    // no delivery ever existed because the bridge registered no route for
    // the event. The ledger trigger vetoes queued→succeeded, so the seed
    // walks the legal path.
    sqlx::query(
        "UPDATE autopilot_actions SET status='processing', started_at=now(), \
         last_error_kind=NULL, updated_at=now() WHERE workspace_id=$1 AND id=$2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .execute(&f.pool)
    .await?;
    sqlx::query(
        "UPDATE autopilot_actions SET status='succeeded', finished_at=now(), \
         updated_at=now() WHERE workspace_id=$1 AND id=$2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .execute(&f.pool)
    .await?;
    let outbox_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO outbox_events (id, workspace_id, event_type, event_version, payload) \
         VALUES ($1,$2,'crowdrelay.beacon.outreach_requested',1,'{}')",
    )
    .bind(outbox_id)
    .bind(f.workspace_id.into_uuid())
    .execute(&f.pool)
    .await?;
    sqlx::query(
        "INSERT INTO autopilot_action_emissions \
         (workspace_id, action_id, emission_key, outbox_event_id) \
         VALUES ($1,$2,$3,$4)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(format!(
        "autopilot-action:{action_id}:crowdrelay.beacon.outreach_requested"
    ))
    .bind(outbox_id)
    .execute(&f.pool)
    .await?;

    let queue = list_beacon_ask_queue(&f.pool, f.workspace_id).await?;
    let item = queue
        .iter()
        .find(|item| item.action_id == action_id)
        .expect("an emitted ask nobody received must resurface");
    assert_eq!(
        item.lane_state,
        crowdrelay_infra::autopilot::BeaconAskLaneState::Unreached
    );

    // Prepare re-verifies but must not re-count the touch dispatch made…
    // this seeded action never completed one, so the campaign row is absent
    // and the letter still builds.
    let prepared = prepare_beacon_ask(&f.pool, f.workspace_id, action_id).await?;
    assert!(prepared.body.contains("Lane night"));
    assert_eq!(action_status(&f, action_id).await?, "succeeded");

    assert!(!operator_send(&f, action_id).await?);
    assert_eq!(action_status(&f, action_id).await?, "succeeded");
    Ok(())
}
