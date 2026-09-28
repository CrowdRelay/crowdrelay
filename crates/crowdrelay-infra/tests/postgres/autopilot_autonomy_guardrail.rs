//! The autonomy guardrail demotes a policy on evidence about its actions.
//!
//! Two consecutive `worsened` outcomes in a context drop its policy from
//! `bounded_auto` to `require_approval` for a week. On 2026-09-26 three
//! `signal_installs_1d` readings — every push endpoint created in the day
//! after an action, 0 against a baseline of 3 — demoted `content_supply`.
//! That count is the whole workspace's, not the action's, so it must not.

use crate::common;
use crowdrelay_application::autopilot::{
    AutopilotMeasurementKind, AutopilotMeasurementRepository, ClaimedAutopilotMeasurement,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::ids::{AutopilotActionId, AutopilotMeasurementId};
use crowdrelay_domain::performance::{EffectAssessment, EffectResult};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
}

async fn setup() -> Result<Fixture, Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $2)")
        .bind(workspace_id.into_uuid())
        .bind(format!("guardrail-{}", workspace_id.into_uuid().simple()))
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
    })
}

/// One succeeded action in `growth_metrics` with a processing measurement.
async fn measured_action(
    f: &Fixture,
    kind: AutopilotMeasurementKind,
    finished_at: OffsetDateTime,
) -> Result<ClaimedAutopilotMeasurement, Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_decisions
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
    .bind(Uuid::now_v7())
    .execute(&f.pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, action_class, finished_at)
           VALUES ($1,$2,$3,'growth_metrics','agent.run.request','target_community',
                   $4,$5,'{}'::jsonb,'succeeded','third_party',$6)"#,
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .bind(format!("idem-{action_id}"))
    .bind(finished_at)
    .execute(&f.pool)
    .await?;
    let id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_measurements
           (id, workspace_id, action_id, measurement_kind, subject_id,
            action_finished_at, baseline_value, due_at, available_at, status, started_at)
           VALUES ($1,$2,$3,$4,$3,$5,3,$5,$5,'processing',now())"#,
    )
    .bind(id)
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(kind.as_str())
    .bind(finished_at)
    .execute(&f.pool)
    .await?;
    Ok(ClaimedAutopilotMeasurement {
        id: AutopilotMeasurementId::from(id),
        action_id: AutopilotActionId::from(action_id),
        kind,
        subject_id: action_id,
        baseline_value: 3.0,
        action_finished_at: finished_at,
        due_at: finished_at,
        attempt_number: 1,
    })
}

async fn worsened(
    f: &Fixture,
    kind: AutopilotMeasurementKind,
    at: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let measurement = measured_action(f, kind, at - time::Duration::days(1)).await?;
    f.repository
        .complete_measurement(
            f.workspace_id,
            &measurement,
            0.0,
            EffectResult {
                assessment: EffectAssessment::Worsened,
                delta_basis_points: -10_000,
            },
            None,
            at,
        )
        .await?;
    Ok(())
}

async fn autonomy(f: &Fixture) -> Result<String, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar(
        "SELECT autonomy_level FROM autopilot_policies WHERE workspace_id = $1 AND context = 'growth_metrics'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await?)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn workspace_window_readings_do_not_demote_a_policy() -> Result<(), Box<dyn std::error::Error>>
{
    let f = setup().await?;
    let now = OffsetDateTime::now_utc();
    assert_eq!(autonomy(&f).await?, "bounded_auto");
    for offset in [3, 2, 1] {
        worsened(
            &f,
            AutopilotMeasurementKind::SignalInstalls1d,
            now - time::Duration::minutes(offset),
        )
        .await?;
    }
    assert_eq!(
        autonomy(&f).await?,
        "bounded_auto",
        "a workspace-window count is not evidence about an action"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn two_worsened_action_outcomes_still_demote_a_policy()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let now = OffsetDateTime::now_utc();
    // A workspace-window reading in between neither counts nor breaks the run.
    worsened(
        &f,
        AutopilotMeasurementKind::ContentLinkClicks7d,
        now - time::Duration::minutes(3),
    )
    .await?;
    worsened(
        &f,
        AutopilotMeasurementKind::SignalInstalls1d,
        now - time::Duration::minutes(2),
    )
    .await?;
    worsened(
        &f,
        AutopilotMeasurementKind::ContentLinkClicks7d,
        now - time::Duration::minutes(1),
    )
    .await?;
    assert_eq!(autonomy(&f).await?, "require_approval");
    Ok(())
}
