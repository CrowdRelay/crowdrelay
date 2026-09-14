//! Business invariant for the attribution outbox: one resolved outcome
//! credits every competing action, and a request that can never succeed
//! does not retry forever.
//!
//! The production failure this pins down ran silently for weeks: two
//! `viryaos_attribution_requests` rows passed 2,000 attempts each because
//! proportional credit wrote one ledger row per competing action while a
//! leftover unique index from migration 0167 allowed only one row per
//! (measurement_id, attribution_version). Every second insert raised
//! unique_violation, the worker mapped it to `pending`, and the next poll
//! retried the same guaranteed failure.

use crowdrelay_application::autopilot::AutopilotDecisionRepository;
use crowdrelay_domain::WorkspaceId;
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
        .bind(format!("attribution-{suffix}"))
        .bind("Attribution Outbox Tests")
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

async fn insert_action(f: &Fixture, dispatched_at: OffsetDateTime) -> uuid::Uuid {
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
    action_id
}

async fn insert_treatment_evidence(
    f: &Fixture,
    action_id: uuid::Uuid,
    timestamp: OffsetDateTime,
    resolved_at: Option<OffsetDateTime>,
    observed_incremental: Option<f64>,
) {
    sqlx::query(
        r#"INSERT INTO viryaos_growth_evidence
           (workspace_id, action_id, opportunity_id, timestamp, recipient_id,
            channel, estimated_reach, treatment, propensity, converted,
            predicted_fans, predicted_signal_installs, context, evidence_quality,
            observed_incremental_fans, resolved_at)
           VALUES ($1,$2,$3,$4,'recipient','reddit_post',100,'treatment',0.9,false,
                   2.0,1.0,'{}'::jsonb,'observational',$5,$6)"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(format!("opp-{action_id}"))
    .bind(timestamp)
    .bind(observed_incremental)
    .bind(resolved_at)
    .execute(&f.pool)
    .await
    .expect("evidence");
}

/// Two competing actions share the outcome's window: the allocator emits a
/// credit per competitor, and every one of them must land. Under the stale
/// (measurement_id, attribution_version) uniqueness the second insert was a
/// unique_violation — and the worker retried that permanent verdict forever.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn credits_for_every_competing_action_land_and_close_the_request() {
    let f = setup().await.expect("disposable test database");
    let focal_action = insert_action(&f, f.now - time::Duration::days(3)).await;
    insert_treatment_evidence(
        &f,
        focal_action,
        f.now - time::Duration::days(3),
        Some(f.now),
        Some(4.0),
    )
    .await;
    for offset in [1_i64, 2_i64] {
        let competitor = insert_action(&f, f.now - time::Duration::days(offset)).await;
        insert_treatment_evidence(
            &f,
            competitor,
            f.now - time::Duration::days(offset),
            None,
            None,
        )
        .await;
    }
    let measurement_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_measurements
           (id, workspace_id, action_id, measurement_kind, subject_id,
            action_finished_at, baseline_value, due_at, available_at, status,
            finished_at)
           VALUES ($1,$2,$3,'incremental_fan_growth_3d',$3,$4,1.0,$4,$4,'succeeded',$4)"#,
    )
    .bind(measurement_id)
    .bind(f.workspace_id.into_uuid())
    .bind(focal_action)
    .bind(f.now)
    .execute(&f.pool)
    .await
    .expect("measurement");
    sqlx::query(
        r#"INSERT INTO viryaos_attribution_requests
           (workspace_id, measurement_id, action_id, attribution_version)
           VALUES ($1,$2,$3,1)"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(measurement_id)
    .bind(focal_action)
    .execute(&f.pool)
    .await
    .expect("request");

    let processed = f
        .repository
        .process_attribution_batch(f.workspace_id, 16)
        .await
        .expect("batch");

    assert_eq!(processed, 1);
    let status: String = sqlx::query_scalar(
        "SELECT status FROM viryaos_attribution_requests WHERE measurement_id = $1",
    )
    .bind(measurement_id)
    .fetch_one(&f.pool)
    .await
    .expect("status");
    assert_eq!(status, "done");
    let credits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_fan_credit_ledger WHERE measurement_id = $1",
    )
    .bind(measurement_id)
    .fetch_one(&f.pool)
    .await
    .expect("credits");
    assert_eq!(credits, 2, "every competing action keeps its credit row");
}
