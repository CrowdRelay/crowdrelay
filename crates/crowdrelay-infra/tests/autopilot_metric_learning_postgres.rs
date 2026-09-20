//! Business invariants for the generic metric write-back.
//!
//! The typed fan-growth columns (`observed_incremental_fans`,
//! `durable_fans_30d`, …) feed dedicated posteriors. Every other measured
//! outcome — ticket revenue, clicks, replies, engagement — lands in the
//! evidence row's `observed_metrics` map, which the metric posteriors replay.
//! These tests pin down the write side: the merge keeps earlier keys, each
//! kind lands under its own `learnable_metric_key`, and the typed kinds never
//! double-write into the map.

use crowdrelay_application::autopilot::{
    AutopilotMeasurementKind, AutopilotMeasurementRepository, ClaimedAutopilotMeasurement,
    assess_measurement_effect,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::ids::{AutopilotActionId, AutopilotMeasurementId};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;
use time::OffsetDateTime;

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
        .bind(format!("metric-learn-{suffix}"))
        .bind("Metric Learning Tests")
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

/// A decision, an action and the growth evidence row a dispatch writes.
async fn insert_dispatch(
    f: &Fixture,
    opportunity_id: &str,
    dispatched_at: OffsetDateTime,
) -> uuid::Uuid {
    let decision_id = uuid::Uuid::now_v7();
    let action_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id)
           VALUES ($1,$2,$3,'growth_metrics','target_community',$4,
                   'auto_execute',9000,'auto_execute','test',
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,gen_random_uuid())"#,
    )
    .bind(decision_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("key-{action_id}"))
    .bind(uuid::Uuid::now_v7())
    .execute(&f.pool)
    .await
    .expect("decision");
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, action_class, finished_at)
           VALUES ($1,$2,$3,'growth_metrics','agent.run.request','target_community',
                   $4,$5,'{}'::jsonb,'succeeded','third_party',$6)"#,
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(uuid::Uuid::now_v7())
    .bind(format!("idem-{action_id}"))
    .bind(dispatched_at)
    .execute(&f.pool)
    .await
    .expect("action");
    sqlx::query(
        r#"INSERT INTO viryaos_growth_evidence
           (workspace_id, action_id, opportunity_id, timestamp, recipient_id,
            channel, estimated_reach, treatment, propensity, converted,
            predicted_fans, predicted_signal_installs, context, evidence_quality)
           VALUES ($1,$2,$3,$4,'recipient','reddit_post',100,'treatment',0.9,false,
                   2.0,1.0,'{}'::jsonb,'observational')"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(opportunity_id)
    .bind(dispatched_at)
    .execute(&f.pool)
    .await
    .expect("evidence");
    action_id
}

/// Queues one measurement and hands back the claim shape the worker would see.
async fn queue_measurement(
    f: &Fixture,
    action_id: uuid::Uuid,
    kind: AutopilotMeasurementKind,
    baseline_value: f64,
    action_finished_at: OffsetDateTime,
) -> ClaimedAutopilotMeasurement {
    let id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_measurements
           (id, workspace_id, action_id, measurement_kind, subject_id,
            action_finished_at, baseline_value, due_at, available_at)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$8)"#,
    )
    .bind(id)
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(kind.as_str())
    .bind(action_id)
    .bind(action_finished_at)
    .bind(baseline_value)
    .bind(action_finished_at + time::Duration::days(7))
    .execute(&f.pool)
    .await
    .expect("measurement");
    ClaimedAutopilotMeasurement {
        id: AutopilotMeasurementId::from(id),
        action_id: AutopilotActionId::from(action_id),
        kind,
        subject_id: action_id,
        baseline_value,
        action_finished_at,
        attempt_number: 1,
    }
}

/// Observes and completes one measurement the way the worker loop does, so the
/// tests exercise the real classification step rather than a hand-made effect.
async fn resolve(f: &Fixture, measurement: &ClaimedAutopilotMeasurement, observed: f64) {
    sqlx::query(
        "UPDATE viryaos_autopilot_measurements SET status='processing', started_at=now() \
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(measurement.id.into_uuid())
    .execute(&f.pool)
    .await
    .expect("processing");
    let effect = assess_measurement_effect(measurement, observed)
        .expect("a measurement the worker can classify");
    f.repository
        .complete_measurement(f.workspace_id, measurement, observed, effect, f.now)
        .await
        .expect("complete");
}

/// H: a measured outcome with no typed column still reaches the evidence row.
///
/// Ticket revenue, clicks, replies and friends used to resolve into
/// `viryaos_autopilot_outcomes` and stop there — assessed, stored, and
/// invisible to every learner. `observed_metrics` is the general write-back:
/// each learnable kind lands under its own key, a merge preserves the keys
/// already present, and a second measurement of the same kind cannot rewrite
/// the first observation.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn generic_metrics_merge_into_evidence() {
    let f = setup().await.expect("fixture");
    let action_id = insert_dispatch(&f, "opp-metrics", f.now - time::Duration::days(30)).await;

    async fn metrics(f: &Fixture, action_id: uuid::Uuid) -> serde_json::Value {
        sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT observed_metrics FROM viryaos_growth_evidence \
             WHERE workspace_id=$1 AND action_id=$2",
        )
        .bind(f.workspace_id.into_uuid())
        .bind(action_id)
        .fetch_one(&f.pool)
        .await
        .expect("evidence metrics")
    }

    // Before anything resolves the map is empty — a dispatch does not
    // pre-populate zeros for metrics nobody measured yet.
    assert_eq!(metrics(&f, action_id).await, serde_json::json!({}));

    let revenue = queue_measurement(
        &f,
        action_id,
        AutopilotMeasurementKind::TicketRevenue72h,
        0.0,
        f.now,
    )
    .await;
    resolve(&f, &revenue, 4200.0).await;
    assert_eq!(
        metrics(&f, action_id).await["ticket_revenue_minor"].as_f64(),
        Some(4200.0),
        "a revenue measurement lands under its learnable key"
    );

    // A different kind merges beside it rather than replacing the map.
    let clicks = queue_measurement(
        &f,
        action_id,
        AutopilotMeasurementKind::ShowGrowthSurfaceClicks7d,
        0.0,
        f.now,
    )
    .await;
    resolve(&f, &clicks, 17.0).await;
    let merged = metrics(&f, action_id).await;
    assert_eq!(merged["ticket_revenue_minor"].as_f64(), Some(4200.0));
    assert_eq!(
        merged["show_growth_clicks"].as_f64(),
        Some(17.0),
        "a second metric merges without losing the first"
    );
    assert_eq!(
        merged.as_object().map(serde_json::Map::len),
        Some(2),
        "and nothing else lands in the map"
    );
}

/// I: a fan-growth kind keeps its value in its typed column only.
///
/// The fan-growth kinds return `None` from `learnable_metric_key` on purpose:
/// they already write `observed_incremental_fans` / `observed_signal_installs`,
/// and a value stored in two places is a value a learner can count twice.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn typed_kinds_do_not_double_write_metrics() {
    let f = setup().await.expect("fixture");
    let action_id = insert_dispatch(&f, "opp-typed", f.now - time::Duration::days(30)).await;

    let growth = queue_measurement(
        &f,
        action_id,
        AutopilotMeasurementKind::IncrementalFanGrowth14d,
        0.0,
        f.now,
    )
    .await;
    resolve(&f, &growth, 5.0).await;

    let (incremental, metrics): (Option<f64>, serde_json::Value) = sqlx::query_as(
        "SELECT observed_incremental_fans, observed_metrics \
         FROM viryaos_growth_evidence WHERE workspace_id=$1 AND action_id=$2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("evidence");
    assert_eq!(
        incremental,
        Some(5.0),
        "the typed column still gets the value"
    );
    assert_eq!(
        metrics,
        serde_json::json!({}),
        "a typed kind leaves observed_metrics empty — no double learning"
    );
}
