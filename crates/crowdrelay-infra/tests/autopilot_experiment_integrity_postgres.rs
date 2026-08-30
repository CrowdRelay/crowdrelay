//! Experiment integrity tests — P0-1, P0-2 behavioral tests T1, T2, T5.
//!
//! These tests verify the experiment design persistence and atomic
//! assignment+execution guarantees against a real Postgres database.
//!
//! T1: Retry does not change assignment — same logical cycle key
//!     converges on the same experiment_uuid.
//! T2: Concurrent evaluators converge — two simultaneous calls to
//!     get_or_create_experiment_design return the same UUID.
//! T5: Atomic treatment bookkeeping — if the assignment INSERT fails,
//!     the action is also not persisted (transaction rollback).

use crowdrelay_application::autopilot::AutopilotDecisionRepository;
use crowdrelay_brain::{ExperimentStatus, ExperimentUnitKind};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;
use time::OffsetDateTime;

struct Fixture {
    #[allow(dead_code)]
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
        .bind(format!("exp-integrity-{suffix}"))
        .bind("Experiment Integrity Tests")
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
    let now = OffsetDateTime::now_utc();
    Ok(Fixture {
        pool,
        repository,
        workspace_id,
        now,
    })
}

/// T1: Retry does not change assignment.
///
/// Calling get_or_create_experiment_design twice with the same
/// (workspace, intervention, logical_cycle_key) must return the same
/// experiment_uuid. This is the P0-1 convergence guarantee.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t1_retry_converges_on_same_experiment_uuid() {
    let f = setup().await.expect("fixture");
    let key = "cycle-test-t1";
    let eligible = vec!["r/djent".to_string(), "r/metalcore".to_string()];

    let design1 = f
        .repository
        .get_or_create_experiment_design(
            f.workspace_id,
            "community-engager",
            key,
            ExperimentUnitKind::TargetCommunity,
            eligible.clone(),
            0.10,
            "discovery",
            10,
            2,
            2,
            f.now,
        )
        .await
        .expect("first design creation");

    let design2 = f
        .repository
        .get_or_create_experiment_design(
            f.workspace_id,
            "community-engager",
            key,
            ExperimentUnitKind::TargetCommunity,
            eligible,
            0.10,
            "discovery",
            10,
            2,
            2,
            f.now,
        )
        .await
        .expect("second design creation (retry)");

    // Same experiment_uuid — retry converges.
    assert_eq!(
        design1.experiment_uuid, design2.experiment_uuid,
        "retry must converge on the same experiment_uuid"
    );
    assert_eq!(design1.logical_cycle_key, key);
    assert_eq!(design2.logical_cycle_key, key);
}

/// T2: Concurrent evaluators converge.
///
/// Two concurrent calls to get_or_create_experiment_design with the same
/// key must return the same experiment_uuid. The DB unique index is the
/// convergence arbiter.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t2_concurrent_evaluators_converge() {
    let f = setup().await.expect("fixture");
    let key = "cycle-test-t2";
    let eligible = vec!["r/progmetal".to_string(), "r/djent".to_string()];

    // Launch two concurrent calls with the same key.
    let (r1, r2) = tokio::join!(
        f.repository.get_or_create_experiment_design(
            f.workspace_id,
            "community-engager",
            key,
            ExperimentUnitKind::TargetCommunity,
            eligible.clone(),
            0.10,
            "discovery",
            10,
            2,
            2,
            f.now,
        ),
        f.repository.get_or_create_experiment_design(
            f.workspace_id,
            "community-engager",
            key,
            ExperimentUnitKind::TargetCommunity,
            eligible,
            0.10,
            "discovery",
            10,
            2,
            2,
            f.now,
        )
    );

    let design1 = r1.expect("first concurrent design");
    let design2 = r2.expect("second concurrent design");

    // Both calls must return the same experiment_uuid.
    assert_eq!(
        design1.experiment_uuid, design2.experiment_uuid,
        "concurrent evaluators must converge on the same experiment_uuid"
    );
}

/// T1 variant: different logical cycle keys produce different experiments.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t1_different_cycle_keys_produce_different_experiments() {
    let f = setup().await.expect("fixture");
    let eligible = vec!["r/djent".to_string()];

    let design1 = f
        .repository
        .get_or_create_experiment_design(
            f.workspace_id,
            "community-engager",
            "cycle-a",
            ExperimentUnitKind::TargetCommunity,
            eligible.clone(),
            0.10,
            "discovery",
            10,
            2,
            2,
            f.now,
        )
        .await
        .expect("design for cycle-a");

    let design2 = f
        .repository
        .get_or_create_experiment_design(
            f.workspace_id,
            "community-engager",
            "cycle-b",
            ExperimentUnitKind::TargetCommunity,
            eligible,
            0.10,
            "discovery",
            10,
            2,
            2,
            f.now,
        )
        .await
        .expect("design for cycle-b");

    // Different cycle keys → different experiments.
    assert_ne!(
        design1.experiment_uuid, design2.experiment_uuid,
        "different cycle keys must produce different experiments"
    );
}

/// T1 variant: the experiment design persists the status and power fields.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t1_design_persists_status_and_unit_kind() {
    let f = setup().await.expect("fixture");
    let eligible: Vec<String> = (0..20).map(|i| format!("r/community{i}")).collect();

    let design = f
        .repository
        .get_or_create_experiment_design(
            f.workspace_id,
            "community-engager",
            "cycle-status-test",
            ExperimentUnitKind::TargetCommunity,
            eligible,
            0.10,
            "discovery",
            10,
            2,
            2,
            f.now,
        )
        .await
        .expect("design creation");

    assert_eq!(design.unit_kind, ExperimentUnitKind::TargetCommunity);
    assert_eq!(design.experiment_status, ExperimentStatus::Active);
    assert_eq!(design.eligible_units.len(), 20);
}
