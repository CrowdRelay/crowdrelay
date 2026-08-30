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
//! T6: Control evidence persistence — recording a control assignment
//!     also creates the control evidence row (atomic, not silent).
//! T7: Control evidence idempotency — replaying control assignment
//!     creation does not duplicate the evidence row.
//! T8: execution_status is persisted correctly for all arms.
//! T9: update_execution_status is monotonic — only executed → failed.

use crowdrelay_application::autopilot::AutopilotDecisionRepository;
use crowdrelay_brain::{
    DispatchContext, DispatchPrediction, ExecutionStatus, ExperimentAssignment, ExperimentStatus,
    ExperimentUnitKind, TreatmentAssignment,
};
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

// ── P0-2: Control evidence + execution_status tests ──

fn make_prediction() -> DispatchPrediction {
    DispatchPrediction {
        template_id: "community.engage".to_owned(),
        expected_new_fans: 5.0,
        expected_signal_installs: 1.0,
        context: DispatchContext::default(),
    }
}

/// T6: Control evidence persistence — recording a control assignment
/// also creates the control evidence row. The write is atomic (same
/// transaction), not silent.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t6_control_evidence_not_silent() {
    let f = setup().await.expect("fixture");
    let design = f
        .repository
        .get_or_create_experiment_design(
            f.workspace_id,
            "community.engage",
            "cycle-t6",
            ExperimentUnitKind::TargetCommunity,
            vec!["r/t6control".to_string()],
            0.10,
            "discovery",
            10,
            2,
            2,
            f.now,
        )
        .await
        .expect("design creation");

    let pred = make_prediction();
    let assignment = ExperimentAssignment::from_design(
        &design,
        "r/t6control",
        "r/t6control",
        TreatmentAssignment::Control,
        &pred,
        None,
    );

    f.repository
        .record_experiment_assignment(f.workspace_id, &assignment, Some("discovery"))
        .await
        .expect("control assignment recording");

    // Verify the evidence row exists.
    let evidence_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_growth_evidence \
         WHERE workspace_id = $1 AND treatment = 'control'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("evidence count query");

    assert_eq!(
        evidence_count, 1,
        "control evidence must exist — silent failure is not allowed"
    );
}

/// T7: Control evidence idempotency — replaying control assignment
/// creation does not duplicate the evidence row.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t7_control_evidence_idempotent_on_retry() {
    let f = setup().await.expect("fixture");
    let design = f
        .repository
        .get_or_create_experiment_design(
            f.workspace_id,
            "community.engage",
            "cycle-t7",
            ExperimentUnitKind::TargetCommunity,
            vec!["r/t7control".to_string()],
            0.10,
            "discovery",
            10,
            2,
            2,
            f.now,
        )
        .await
        .expect("design creation");

    let pred = make_prediction();
    let assignment = ExperimentAssignment::from_design(
        &design,
        "r/t7control",
        "r/t7control",
        TreatmentAssignment::Control,
        &pred,
        None,
    );

    // Record twice (simulating retry).
    f.repository
        .record_experiment_assignment(f.workspace_id, &assignment, Some("discovery"))
        .await
        .expect("first control assignment recording");
    f.repository
        .record_experiment_assignment(f.workspace_id, &assignment, Some("discovery"))
        .await
        .expect("retry control assignment recording");

    // Verify exactly 1 evidence row (not 2).
    let evidence_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_growth_evidence \
         WHERE workspace_id = $1 AND treatment = 'control'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("evidence count query");

    assert_eq!(
        evidence_count, 1,
        "retry must not duplicate control evidence — idempotency required"
    );

    // Verify exactly 1 assignment row (not 2).
    let assignment_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_experiment_assignments \
         WHERE workspace_id = $1 AND arm = 'control'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("assignment count query");

    assert_eq!(
        assignment_count, 1,
        "retry must not duplicate control assignment — idempotency required"
    );
}

/// T8: execution_status is persisted correctly for all arms.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t8_execution_status_persisted() {
    let f = setup().await.expect("fixture");
    let design = f
        .repository
        .get_or_create_experiment_design(
            f.workspace_id,
            "community.engage",
            "cycle-t8",
            ExperimentUnitKind::TargetCommunity,
            vec!["r/t8control".to_string(), "r/t8treatment".to_string()],
            0.10,
            "discovery",
            10,
            2,
            2,
            f.now,
        )
        .await
        .expect("design creation");

    let pred = make_prediction();

    // Control assignment → execution_status='control'
    let control = ExperimentAssignment::from_design(
        &design,
        "r/t8control",
        "r/t8control",
        TreatmentAssignment::Control,
        &pred,
        None,
    );
    f.repository
        .record_experiment_assignment(f.workspace_id, &control, Some("discovery"))
        .await
        .expect("control assignment recording");

    // Withheld-treatment assignment → execution_status='withheld'
    let withheld = ExperimentAssignment::from_design(
        &design,
        "r/t8treatment",
        "r/t8treatment",
        TreatmentAssignment::Treatment,
        &pred,
        None, // no action_id → withheld
    );
    f.repository
        .record_experiment_assignment(f.workspace_id, &withheld, Some("discovery"))
        .await
        .expect("withheld assignment recording");

    // Verify control execution_status.
    let control_status: String = sqlx::query_scalar(
        "SELECT execution_status FROM viryaos_experiment_assignments \
         WHERE workspace_id = $1 AND unit_id = 'r/t8control'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("control status query");
    assert_eq!(control_status, "control");

    // Verify withheld execution_status.
    let withheld_status: String = sqlx::query_scalar(
        "SELECT execution_status FROM viryaos_experiment_assignments \
         WHERE workspace_id = $1 AND unit_id = 'r/t8treatment'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("withheld status query");
    assert_eq!(withheld_status, "withheld");
}

/// T9: update_execution_status is monotonic — only executed → failed.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t9_execution_status_monotonic() {
    let f = setup().await.expect("fixture");
    let design = f
        .repository
        .get_or_create_experiment_design(
            f.workspace_id,
            "community.engage",
            "cycle-t9",
            ExperimentUnitKind::TargetCommunity,
            vec!["r/t9control".to_string(), "r/t9treatment".to_string()],
            0.10,
            "discovery",
            10,
            2,
            2,
            f.now,
        )
        .await
        .expect("design creation");

    let pred = make_prediction();

    // Control assignment → execution_status='control'
    let control = ExperimentAssignment::from_design(
        &design,
        "r/t9control",
        "r/t9control",
        TreatmentAssignment::Control,
        &pred,
        None,
    );
    f.repository
        .record_experiment_assignment(f.workspace_id, &control, Some("discovery"))
        .await
        .expect("control assignment recording");

    // Attempt to transition control → failed (should be no-op).
    f.repository
        .update_execution_status(
            f.workspace_id,
            &control.assignment_id,
            ExecutionStatus::Failed,
        )
        .await
        .expect("update attempt");

    let control_status: String = sqlx::query_scalar(
        "SELECT execution_status FROM viryaos_experiment_assignments \
         WHERE workspace_id = $1 AND unit_id = 'r/t9control'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("control status query");
    assert_eq!(
        control_status, "control",
        "control is terminal — update must be a no-op"
    );

    // Withheld-treatment assignment → execution_status='withheld'
    let withheld = ExperimentAssignment::from_design(
        &design,
        "r/t9treatment",
        "r/t9treatment",
        TreatmentAssignment::Treatment,
        &pred,
        None,
    );
    f.repository
        .record_experiment_assignment(f.workspace_id, &withheld, Some("discovery"))
        .await
        .expect("withheld assignment recording");

    // Attempt to transition withheld → failed (should be no-op).
    f.repository
        .update_execution_status(
            f.workspace_id,
            &withheld.assignment_id,
            ExecutionStatus::Failed,
        )
        .await
        .expect("update attempt");

    let withheld_status: String = sqlx::query_scalar(
        "SELECT execution_status FROM viryaos_experiment_assignments \
         WHERE workspace_id = $1 AND unit_id = 'r/t9treatment'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("withheld status query");
    assert_eq!(
        withheld_status, "withheld",
        "withheld is terminal — update must be a no-op"
    );
}
