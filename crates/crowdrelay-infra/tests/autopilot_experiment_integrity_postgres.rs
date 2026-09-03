//! Experiment integrity tests — P0-1, P0-2 behavioral tests T1-T9,
//! execution integrity tests T10-T19.
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
//! T9: update_execution_status is monotonic — only dispatched → executed
//!     and dispatched → failed are allowed.
//! T10: Evidence projection rebuildability — growth_episodes can be
//!      rebuilt from growth_evidence with semantic equality and idempotence.
//! T11: Stale posting transitions to 'unknown', not 'failed' —
//!      confirmation loss is NOT intervention failure.
//! T12: Real failure transitions to 'failed' — definitive execution
//!      failure is counted as a failed treatment.
//! T13: Trace ID continuity across the action lifecycle — the same
//!      trace_id appears in all lifecycle records.
//! T14: Cross-layer invariant — execution certainty and causal treatment
//!      classification cannot contradict one another.
//! T15: SQL/Rust reconciliation parity — the SQL fallback function and
//!      the Rust worker produce identical classification for the same
//!      community_posts fixture states.
//! T16: Trace continuity invariant — existing trace_id MUST propagate;
//!      missing trace_id MUST NOT fabricate fake continuity.
//! T17: One growth episode per action — the source-of-truth evidence
//!      cannot produce multiple conflicting episodes for the same action.
//! T18: INSERT-vs-SELECT strictness — newly inserted rows may use safe
//!      construction defaults; existing persisted rows must be read
//!      strictly with zero fallback tolerance.
//! T19: Full chain 3-branch proof — SUCCESS, FAILURE, and CONFIRMATION
//!      LOSS each produce the correct learning interpretation across
//!      the entire chain: assignment → action → evidence → episode →
//!      trace → ledger → learner interpretation.
//! T26: Concurrent resolution race — two workers, one action, one winner.
//!      The WHERE status = 'unknown' guard ensures no split-brain.
//! T27: Contradictory provider facts — success receipt for a FAILED
//!      action → Conflict, no state change. The resolver surfaces the
//!      contradiction instead of silently reviving.
//! North Star A: success → confirmation lost → UNKNOWN → recovery →
//!      exactly one effect. Proves the full loss-and-recovery lifecycle
//!      produces no duplication.
//! North Star B: UNKNOWN → definitive non-execution → FAILED → safe
//!      retry → one effect. Proves that a failed action stays failed
//!      and a retry can succeed without reviving the original.

use crowdrelay_application::autopilot::{
    AutopilotDecisionRepository, AutopilotRuntimeRepository, ClaimExecution, ExecutorReportStatus,
    RecordExecutionReport,
};
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
    // `RepositoryError` carries no payload, so a failure here reads only as
    // `Err(Unexpected)` and says nothing about what the database refused.
    // `map_sqlx` already logs the real cause — it just had no subscriber to log
    // to, which is a large part of why this suite became unreadable enough to
    // quarantine. `try_init` is the idempotent form; every test calls `setup`.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_test_writer()
        .try_init();
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
        target_key: Some("community:test".to_owned()),
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

/// T9: update_execution_status is monotonic — only dispatched → executed
/// and dispatched → failed are allowed. All other transitions are no-ops.
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

include!("autopilot_experiment_integrity_postgres/fixtures.rs");

/// T10: Evidence projection rebuildability — growth_episodes can be
/// rebuilt from growth_evidence with semantic equality, and the rebuild
/// is idempotent: rebuild(E) == rebuild(rebuild(E)).
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t10_evidence_episodes_rebuildable() {
    let f = setup().await.expect("fixture");
    let trace_id = uuid::Uuid::now_v7();
    let action_id = insert_decision_and_action(&f.pool, f.workspace_id, trace_id).await;

    let design = f
        .repository
        .get_or_create_experiment_design(
            f.workspace_id,
            "community.engage",
            "cycle-t10",
            ExperimentUnitKind::TargetCommunity,
            vec!["r/t10treatment".to_string()],
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
        "r/t10treatment",
        "r/t10treatment",
        TreatmentAssignment::Treatment,
        &pred,
        Some(action_id),
    );
    f.repository
        .record_experiment_assignment(f.workspace_id, &assignment, Some("discovery"))
        .await
        .expect("treatment assignment recording");

    // Verify the episode was created.
    let episode_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_growth_episodes \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("episode count");
    assert_eq!(episode_count, 1, "episode must exist after assignment");

    // Capture the original episode state.
    let original: (String, f64, f64, f64, f64, Option<i32>, bool) = sqlx::query_as(
        "SELECT treatment, propensity, predicted_fans, predicted_signal_installs, \
                observed_fans, actual_reach, converted \
         FROM viryaos_growth_episodes \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("original episode");

    // Delete all episodes.
    sqlx::query("DELETE FROM viryaos_growth_episodes WHERE workspace_id = $1")
        .bind(f.workspace_id.into_uuid())
        .execute(&f.pool)
        .await
        .expect("delete episodes");

    let count_after_delete: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM viryaos_growth_episodes WHERE workspace_id = $1")
            .bind(f.workspace_id.into_uuid())
            .fetch_one(&f.pool)
            .await
            .expect("count after delete");
    assert_eq!(count_after_delete, 0, "episodes must be deleted");

    // Rebuild from evidence.
    let rebuilt_count: i32 =
        sqlx::query_scalar("SELECT viryaos_rebuild_growth_episodes_from_evidence($1)")
            .bind(f.workspace_id.into_uuid())
            .fetch_one(&f.pool)
            .await
            .expect("rebuild");
    assert_eq!(rebuilt_count, 1, "rebuild must produce 1 episode");

    // Verify semantic equality.
    let rebuilt: (String, f64, f64, f64, f64, Option<i32>, bool) = sqlx::query_as(
        "SELECT treatment, propensity, predicted_fans, predicted_signal_installs, \
                observed_fans, actual_reach, converted \
         FROM viryaos_growth_episodes \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("rebuilt episode");

    assert_eq!(rebuilt.0, original.0, "treatment must match");
    assert!(
        (rebuilt.1 - original.1).abs() < 1e-10,
        "propensity must match"
    );
    assert!(
        (rebuilt.2 - original.2).abs() < 1e-10,
        "predicted_fans must match"
    );
    assert!(
        (rebuilt.3 - original.3).abs() < 1e-10,
        "predicted_signal_installs must match"
    );
    assert!(
        (rebuilt.4 - original.4).abs() < 1e-10,
        "observed_fans must match"
    );
    assert_eq!(rebuilt.5, original.5, "actual_reach must match");
    assert_eq!(rebuilt.6, original.6, "converted must match");

    // Idempotence: rebuild again and verify the same state.
    let rebuilt_count_2: i32 =
        sqlx::query_scalar("SELECT viryaos_rebuild_growth_episodes_from_evidence($1)")
            .bind(f.workspace_id.into_uuid())
            .fetch_one(&f.pool)
            .await
            .expect("rebuild 2");
    assert_eq!(
        rebuilt_count_2, 1,
        "second rebuild must also produce 1 episode"
    );

    let rebuilt2: (String, f64, f64, f64, f64, Option<i32>, bool) = sqlx::query_as(
        "SELECT treatment, propensity, predicted_fans, predicted_signal_installs, \
                observed_fans, actual_reach, converted \
         FROM viryaos_growth_episodes \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("rebuilt episode 2");

    assert_eq!(rebuilt2.0, rebuilt.0, "idempotence: treatment");
    assert!(
        (rebuilt2.1 - rebuilt.1).abs() < 1e-10,
        "idempotence: propensity"
    );
    assert!(
        (rebuilt2.2 - rebuilt.2).abs() < 1e-10,
        "idempotence: predicted_fans"
    );
    assert!(
        (rebuilt2.4 - rebuilt.4).abs() < 1e-10,
        "idempotence: observed_fans"
    );
}

/// T11: Stale posting transitions to 'unknown', not 'failed'.
///
/// When a worker crashes during the Reddit API call, the community_posts
/// row is marked 'failed' (the DB record failed), but the autopilot action
/// is transitioned to 'unknown' (NOT 'failed') because the Reddit post may
/// have actually succeeded. The experiment assignment is also transitioned
/// to 'unknown', which excludes it from both realized-treatment and
/// failed-treatment counts.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t11_stale_posting_transitions_to_unknown_not_failed() {
    let f = setup().await.expect("fixture");
    let trace_id = uuid::Uuid::now_v7();
    let action_id = insert_decision_and_action(&f.pool, f.workspace_id, trace_id).await;

    let design = f
        .repository
        .get_or_create_experiment_design(
            f.workspace_id,
            "community.engage",
            "cycle-t11",
            ExperimentUnitKind::TargetCommunity,
            vec!["r/t11treatment".to_string()],
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
        "r/t11treatment",
        "r/t11treatment",
        TreatmentAssignment::Treatment,
        &pred,
        Some(action_id),
    );
    f.repository
        .record_experiment_assignment(f.workspace_id, &assignment, Some("discovery"))
        .await
        .expect("treatment assignment recording");

    // Simulate a stale posting: community_posts in 'posting' with an old timestamp.
    let post_id =
        insert_community_post(&f.pool, f.workspace_id, action_id, "posting", "r/t11").await;
    // Set the updated_at to 10 minutes ago (past the 5-minute stale threshold).
    sqlx::query(
        "UPDATE community_posts SET updated_at = now() - interval '10 minutes' WHERE id = $1",
    )
    .bind(post_id)
    .execute(&f.pool)
    .await
    .expect("set stale timestamp");

    // Simulate recover_stale_posting: mark community_posts as 'failed',
    // but transition the autopilot action to 'unknown'.
    sqlx::query(
        r#"UPDATE community_posts
           SET status = 'failed',
               error_message = 'worker crashed during posting — check Reddit manually',
               updated_at = now()
           WHERE id = $1"#,
    )
    .bind(post_id)
    .execute(&f.pool)
    .await
    .expect("mark post failed");

    sqlx::query(
        r#"UPDATE viryaos_autopilot_actions
           SET status = 'unknown', finished_at = NULL, updated_at = now()
           WHERE id = $1 AND status IN ('succeeded', 'processing')"#,
    )
    .bind(action_id)
    .execute(&f.pool)
    .await
    .expect("mark action unknown");

    sqlx::query(
        r#"UPDATE viryaos_experiment_assignments
           SET execution_status = 'unknown'
           WHERE workspace_id = $1 AND action_id = $2 AND execution_status = 'dispatched'"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .execute(&f.pool)
    .await
    .expect("mark assignment unknown");

    // Verify: community_posts.status = 'failed'
    let post_status: String =
        sqlx::query_scalar("SELECT status FROM community_posts WHERE id = $1")
            .bind(post_id)
            .fetch_one(&f.pool)
            .await
            .expect("post status");
    assert_eq!(post_status, "failed", "community_posts must be 'failed'");

    // Verify: autopilot_actions.status = 'unknown'
    let action_status: String =
        sqlx::query_scalar("SELECT status FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("action status");
    assert_eq!(
        action_status, "unknown",
        "action must be 'unknown', not 'failed'"
    );

    // Verify: action_ledger.state = 'UNKNOWN'
    let ledger_state: String =
        sqlx::query_scalar("SELECT state FROM viryaos_action_ledger WHERE action_id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("ledger state");
    assert_eq!(ledger_state, "UNKNOWN", "ledger must be 'UNKNOWN'");

    // Verify: experiment_assignments.execution_status = 'unknown'
    let exec_status: String = sqlx::query_scalar(
        "SELECT execution_status FROM viryaos_experiment_assignments \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("execution status");
    assert_eq!(
        exec_status, "unknown",
        "assignment must be 'unknown' — NOT 'failed' and NOT 'dispatched'"
    );

    // Evidence quality: the growth_evidence record must NOT be classified
    // as a realized treatment failure. Unknown is excluded from both
    // realized-treatment and failed-treatment counts.
    let evidence_treatment: String = sqlx::query_scalar(
        "SELECT treatment FROM viryaos_growth_evidence \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("evidence treatment");
    // The evidence treatment field is 'treatment' (the arm), but the
    // execution_status='unknown' means this is NOT a realized treatment.
    // The causal learner must check execution_status, not just the arm.
    assert_eq!(
        evidence_treatment, "treatment",
        "evidence arm is 'treatment' but execution_status='unknown' means NOT realized"
    );
}

/// T12: Real failure transitions to 'failed'.
///
/// When the executor definitively fails (e.g., NoAgentsService), the
/// autopilot action is transitioned to 'failed', the action ledger to
/// FAILED, and the experiment assignment to 'failed'. This IS a realized
/// treatment failure — the intervention definitively did not happen.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t12_real_failure_transitions_to_failed() {
    let f = setup().await.expect("fixture");
    let trace_id = uuid::Uuid::now_v7();
    let action_id = insert_decision_and_action(&f.pool, f.workspace_id, trace_id).await;

    let design = f
        .repository
        .get_or_create_experiment_design(
            f.workspace_id,
            "community.engage",
            "cycle-t12",
            ExperimentUnitKind::TargetCommunity,
            vec!["r/t12treatment".to_string()],
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
        "r/t12treatment",
        "r/t12treatment",
        TreatmentAssignment::Treatment,
        &pred,
        Some(action_id),
    );
    f.repository
        .record_experiment_assignment(f.workspace_id, &assignment, Some("discovery"))
        .await
        .expect("treatment assignment recording");

    // Simulate a real failure: community_posts marked 'failed',
    // autopilot action → 'failed', assignment → 'failed'.
    let post_id =
        insert_community_post(&f.pool, f.workspace_id, action_id, "posting", "r/t12").await;
    sqlx::query(
        r#"UPDATE community_posts
           SET status = 'failed', error_message = 'no agents service configured',
               updated_at = now()
           WHERE id = $1"#,
    )
    .bind(post_id)
    .execute(&f.pool)
    .await
    .expect("mark post failed");

    sqlx::query(
        r#"UPDATE viryaos_autopilot_actions
           SET status = 'failed', finished_at = now(), updated_at = now()
           WHERE id = $1 AND status = 'succeeded'"#,
    )
    .bind(action_id)
    .execute(&f.pool)
    .await
    .expect("mark action failed");

    sqlx::query(
        r#"UPDATE viryaos_experiment_assignments
           SET execution_status = 'failed'
           WHERE workspace_id = $1 AND action_id = $2 AND execution_status = 'dispatched'"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .execute(&f.pool)
    .await
    .expect("mark assignment failed");

    // Verify: autopilot_actions.status = 'failed'
    let action_status: String =
        sqlx::query_scalar("SELECT status FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("action status");
    assert_eq!(action_status, "failed", "action must be 'failed'");

    // Verify: action_ledger.state = 'FAILED'
    let ledger_state: String =
        sqlx::query_scalar("SELECT state FROM viryaos_action_ledger WHERE action_id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("ledger state");
    assert_eq!(ledger_state, "FAILED", "ledger must be 'FAILED'");

    // Verify: experiment_assignments.execution_status = 'failed'
    let exec_status: String = sqlx::query_scalar(
        "SELECT execution_status FROM viryaos_experiment_assignments \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("execution status");
    assert_eq!(
        exec_status, "failed",
        "assignment must be 'failed' — real failure, not unknown"
    );
}

/// T13: Trace ID continuity across the action lifecycle.
///
/// The same trace_id must appear in all lifecycle records for an action:
/// decision → action → outbox → reach_event → evidence_event → measurement.
/// This proves the trace forms a continuous chain, not just correctly named columns.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t13_trace_id_continuity_across_lifecycle() {
    let f = setup().await.expect("fixture");
    let trace_id = uuid::Uuid::now_v7();
    let action_id = insert_decision_and_action(&f.pool, f.workspace_id, trace_id).await;

    // Insert an outbox event with the same trace_id.
    sqlx::query(
        r#"INSERT INTO outbox_events
           (workspace_id, event_type, event_version, payload, max_attempts, trace_id, action_id)
           VALUES ($1, 'crowdrelay.autopilot.approval_requested', 1,
                   jsonb_build_object('action_id', $2, 'trace_id', $3),
                   12, $3, $2)"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(trace_id)
    .execute(&f.pool)
    .await
    .expect("insert outbox event");

    // Insert a reach_event with the same trace_id.
    sqlx::query(
        r#"INSERT INTO viryaos_reach_events
           (workspace_id, action_id, recipient_kind, recipient_id, channel,
            template_id, estimated_reach, status, trace_id)
           VALUES ($1, $2, 'subreddit_audience', 'r/t13', 'reddit_post',
                   'community-engager', 100, 'delivered', $3)"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(trace_id)
    .execute(&f.pool)
    .await
    .expect("insert reach event");

    // Insert a measurement with the same trace_id.
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_measurements
           (id, workspace_id, action_id, measurement_kind, subject_id,
            action_finished_at, baseline_value, due_at, available_at, trace_id)
           VALUES ($1, $2, $3, 'ticket_revenue_72h', $4, now(), 0.0,
                   now() + interval '7 days', now() + interval '7 days', $5)"#,
    )
    .bind(uuid::Uuid::now_v7())
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(uuid::Uuid::now_v7())
    .bind(trace_id)
    .execute(&f.pool)
    .await
    .expect("insert measurement");

    // Verify: all lifecycle records have the same trace_id.
    let decision_trace: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT trace_id FROM viryaos_autopilot_decisions \
         WHERE workspace_id = $1 AND trace_id = $2 LIMIT 1",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(trace_id)
    .fetch_one(&f.pool)
    .await
    .expect("decision trace");
    assert_eq!(
        decision_trace,
        Some(trace_id),
        "decision must have trace_id"
    );

    let action_trace: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT trace_id FROM viryaos_autopilot_actions \
         WHERE id = $1 AND trace_id = $2",
    )
    .bind(action_id)
    .bind(trace_id)
    .fetch_one(&f.pool)
    .await
    .expect("action trace");
    assert_eq!(action_trace, Some(trace_id), "action must have trace_id");

    let outbox_trace: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT trace_id FROM outbox_events \
         WHERE workspace_id = $1 AND trace_id = $2 AND action_id = $3 LIMIT 1",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(trace_id)
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("outbox trace");
    assert_eq!(outbox_trace, Some(trace_id), "outbox must have trace_id");

    let reach_trace: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT trace_id FROM viryaos_reach_events \
         WHERE workspace_id = $1 AND action_id = $2 AND trace_id = $3",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(trace_id)
    .fetch_one(&f.pool)
    .await
    .expect("reach trace");
    assert_eq!(
        reach_trace,
        Some(trace_id),
        "reach_event must have trace_id"
    );

    let measurement_trace: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT trace_id FROM viryaos_autopilot_measurements \
         WHERE workspace_id = $1 AND action_id = $2 AND trace_id = $3",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(trace_id)
    .fetch_one(&f.pool)
    .await
    .expect("measurement trace");
    assert_eq!(
        measurement_trace,
        Some(trace_id),
        "measurement must have trace_id"
    );

    // Verify: no lifecycle record has a DIFFERENT trace_id for this action.
    let mismatched: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_autopilot_actions \
         WHERE id = $1 AND trace_id IS NOT NULL AND trace_id != $2",
    )
    .bind(action_id)
    .bind(trace_id)
    .fetch_one(&f.pool)
    .await
    .expect("mismatch check");
    assert_eq!(mismatched, 0, "no action should have a different trace_id");
}

/// T14: Cross-layer invariant — execution certainty and causal treatment
/// classification cannot contradict one another.
///
/// Forbidden mappings:
///   Unknown → never Treated (excluded from realized-treatment analysis)
///   Unknown → never Failed (excluded from failed-treatment counts)
///   Executed → Treated (realized treatment)
///   Failed → never Treated (intervention did not happen)
///   Control → never Treated
///   Withheld → never Treated (withheld treatment = control for ITT)
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t14_cross_layer_invariant_forbidden_mappings() {
    let f = setup().await.expect("fixture");
    let trace_id = uuid::Uuid::now_v7();
    let action_id = insert_decision_and_action(&f.pool, f.workspace_id, trace_id).await;

    let design = f
        .repository
        .get_or_create_experiment_design(
            f.workspace_id,
            "community.engage",
            "cycle-t14",
            ExperimentUnitKind::TargetCommunity,
            vec![
                "r/t14control".to_string(),
                "r/t14withheld".to_string(),
                "r/t14executed".to_string(),
                "r/t14failed".to_string(),
                "r/t14unknown".to_string(),
            ],
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

    // Control → never Treated
    let control = ExperimentAssignment::from_design(
        &design,
        "r/t14control",
        "r/t14control",
        TreatmentAssignment::Control,
        &pred,
        None,
    );
    f.repository
        .record_experiment_assignment(f.workspace_id, &control, Some("discovery"))
        .await
        .expect("control assignment");

    // Withheld → never Treated
    let withheld = ExperimentAssignment::from_design(
        &design,
        "r/t14withheld",
        "r/t14withheld",
        TreatmentAssignment::Treatment,
        &pred,
        None, // no action_id → withheld
    );
    f.repository
        .record_experiment_assignment(f.workspace_id, &withheld, Some("discovery"))
        .await
        .expect("withheld assignment");

    // Executed → Treated
    let executed_action = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, approved_at, available_at, finished_at, trace_id)
           SELECT $1, $2, decision_id, 'growth_metrics', 'community.engage.request', 'target_community',
                  $3, $4, '{"kind":"community.engage.request"}', 'succeeded', now(), now(), now(), $5
           FROM viryaos_autopilot_actions WHERE id = $6"#,
    )
    .bind(executed_action)
    .bind(f.workspace_id.into_uuid())
    .bind(uuid::Uuid::now_v7())
    .bind(format!("action-{executed_action}"))
    .bind(uuid::Uuid::now_v7())
    .bind(action_id)
    .execute(&f.pool)
    .await
    .expect("insert executed action");

    let executed = ExperimentAssignment::from_design(
        &design,
        "r/t14executed",
        "r/t14executed",
        TreatmentAssignment::Treatment,
        &pred,
        Some(executed_action),
    );
    f.repository
        .record_experiment_assignment(f.workspace_id, &executed, Some("discovery"))
        .await
        .expect("executed assignment");
    // Transition to executed.
    f.repository
        .update_execution_status_by_action_id(
            f.workspace_id,
            executed_action,
            ExecutionStatus::Executed,
        )
        .await
        .expect("transition to executed");

    // Failed → never Treated
    let failed_action = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, approved_at, available_at, finished_at, trace_id)
           SELECT $1, $2, decision_id, 'growth_metrics', 'community.engage.request', 'target_community',
                  $3, $4, '{"kind":"community.engage.request"}', 'failed', now(), now(), now(), $5
           FROM viryaos_autopilot_actions WHERE id = $6"#,
    )
    .bind(failed_action)
    .bind(f.workspace_id.into_uuid())
    .bind(uuid::Uuid::now_v7())
    .bind(format!("action-{failed_action}"))
    .bind(uuid::Uuid::now_v7())
    .bind(action_id)
    .execute(&f.pool)
    .await
    .expect("insert failed action");

    let failed = ExperimentAssignment::from_design(
        &design,
        "r/t14failed",
        "r/t14failed",
        TreatmentAssignment::Treatment,
        &pred,
        Some(failed_action),
    );
    f.repository
        .record_experiment_assignment(f.workspace_id, &failed, Some("discovery"))
        .await
        .expect("failed assignment");
    f.repository
        .update_execution_status_by_action_id(
            f.workspace_id,
            failed_action,
            ExecutionStatus::Failed,
        )
        .await
        .expect("transition to failed");

    // Unknown → never Treated, never Failed
    let unknown_action = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, approved_at, available_at, trace_id)
           SELECT $1, $2, decision_id, 'growth_metrics', 'community.engage.request', 'target_community',
                  $3, $4, '{"kind":"community.engage.request"}', 'unknown', now(), now(), $5
           FROM viryaos_autopilot_actions WHERE id = $6"#,
    )
    .bind(unknown_action)
    .bind(f.workspace_id.into_uuid())
    .bind(uuid::Uuid::now_v7())
    .bind(format!("action-{unknown_action}"))
    .bind(uuid::Uuid::now_v7())
    .bind(action_id)
    .execute(&f.pool)
    .await
    .expect("insert unknown action");

    let unknown = ExperimentAssignment::from_design(
        &design,
        "r/t14unknown",
        "r/t14unknown",
        TreatmentAssignment::Treatment,
        &pred,
        Some(unknown_action),
    );
    f.repository
        .record_experiment_assignment(f.workspace_id, &unknown, Some("discovery"))
        .await
        .expect("unknown assignment");
    f.repository
        .update_execution_status_by_action_id(
            f.workspace_id,
            unknown_action,
            ExecutionStatus::Unknown,
        )
        .await
        .expect("transition to unknown");

    // ── Verify forbidden mappings ──

    // Control → never Treated: arm='control', execution_status='control'
    let (control_arm, control_exec): (String, String) = sqlx::query_as(
        "SELECT arm::text, execution_status FROM viryaos_experiment_assignments \
         WHERE workspace_id = $1 AND unit_id = 'r/t14control'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("control query");
    assert_eq!(control_arm, "control");
    assert_eq!(control_exec, "control");

    // Withheld → never Treated: arm='treatment', execution_status='withheld'
    let (withheld_arm, withheld_exec): (String, String) = sqlx::query_as(
        "SELECT arm::text, execution_status FROM viryaos_experiment_assignments \
         WHERE workspace_id = $1 AND unit_id = 'r/t14withheld'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("withheld query");
    assert_eq!(withheld_arm, "treatment");
    assert_eq!(withheld_exec, "withheld");

    // Executed → Treated: arm='treatment', execution_status='executed'
    let (executed_arm, executed_exec): (String, String) = sqlx::query_as(
        "SELECT arm::text, execution_status FROM viryaos_experiment_assignments \
         WHERE workspace_id = $1 AND unit_id = 'r/t14executed'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("executed query");
    assert_eq!(executed_arm, "treatment");
    assert_eq!(executed_exec, "executed");

    // Failed → never Treated: arm='treatment', execution_status='failed'
    // (the arm is 'treatment' for ITT, but execution_status='failed' means
    // the intervention did NOT happen — per-protocol excludes it)
    let (failed_arm, failed_exec): (String, String) = sqlx::query_as(
        "SELECT arm::text, execution_status FROM viryaos_experiment_assignments \
         WHERE workspace_id = $1 AND unit_id = 'r/t14failed'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("failed query");
    assert_eq!(failed_arm, "treatment");
    assert_eq!(failed_exec, "failed");

    // Unknown → never Treated, never Failed: arm='treatment', execution_status='unknown'
    let (unknown_arm, unknown_exec): (String, String) = sqlx::query_as(
        "SELECT arm::text, execution_status FROM viryaos_experiment_assignments \
         WHERE workspace_id = $1 AND unit_id = 'r/t14unknown'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("unknown query");
    assert_eq!(unknown_arm, "treatment");
    assert_eq!(
        unknown_exec, "unknown",
        "Unknown must not be Treated or Failed — it is unresolved"
    );

    // ── Verify assignment linkage is deterministic ──
    // Each action_id has at most one assignment.
    let duplicate_assignments: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
            SELECT action_id, COUNT(*) as cnt
            FROM viryaos_experiment_assignments
            WHERE workspace_id = $1 AND action_id IS NOT NULL
            GROUP BY action_id HAVING COUNT(*) > 1
        ) dup",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("duplicate check");
    assert_eq!(
        duplicate_assignments, 0,
        "each action_id must have at most one assignment"
    );

    // ── Verify initial growth evidence exists exactly once per action ──
    let evidence_dupes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
            SELECT action_id, COUNT(*) as cnt
            FROM viryaos_growth_evidence
            WHERE workspace_id = $1 AND action_id IS NOT NULL
            GROUP BY action_id HAVING COUNT(*) > 1
        ) dup",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("evidence duplicate check");
    assert_eq!(
        evidence_dupes, 0,
        "each action_id must have at most one growth evidence record"
    );
}

/// T15: SQL/Rust reconciliation parity.
///
/// The SQL fallback function `viryaos_action_ledger_reconcile` and the
/// Rust `community_post_to_evidence` + `resolve_observation` + `legal_transition` pipeline must
/// produce identical classification for the same community_posts fixture
/// states. This proves the SQL fallback does not have weaker causal
/// semantics than the primary Rust reconciler.
///
/// This test also verifies that the SQL function correctly matches
/// `action_kind = 'community.engage.request'` (the real action_kind
/// produced by the brain). A previous version of the function used
/// `'community.engage'` (missing the `.request` suffix), which meant
/// the community_posts strategy was dead code — the function fell
/// through to the ELSE branch and returned UNKNOWN for all community
/// actions.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t15_sql_rust_reconciliation_parity() {
    let f = setup().await.expect("fixture");

    // We test 4 fixture states and verify the SQL function classifies
    // each correctly. The Rust classification is already tested in
    // receipt_reconciliation.rs unit tests — here we verify the SQL
    // function matches.

    // Fixture 1: posted → SUCCEEDED
    let trace1 = uuid::Uuid::now_v7();
    let action1 = insert_decision_and_action(&f.pool, f.workspace_id, trace1).await;
    insert_community_post(&f.pool, f.workspace_id, action1, "posted", "r/t15a").await;
    // Transition action to unknown so reconciliation can run.
    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status = 'unknown', finished_at = NULL \
         WHERE id = $1",
    )
    .bind(action1)
    .execute(&f.pool)
    .await
    .expect("mark unknown");
    let result1: String = sqlx::query_scalar("SELECT viryaos_action_ledger_reconcile($1)")
        .bind(action1)
        .fetch_one(&f.pool)
        .await
        .expect("reconcile posted");
    assert_eq!(
        result1, "SUCCEEDED",
        "posted community_post must reconcile to SUCCEEDED"
    );

    // Fixture 2: crash-marked failed → UNKNOWN
    let trace2 = uuid::Uuid::now_v7();
    let action2 = insert_decision_and_action(&f.pool, f.workspace_id, trace2).await;
    insert_community_post_with_error(
        &f.pool,
        f.workspace_id,
        action2,
        "failed",
        "r/t15b",
        "worker crashed during posting — check Reddit manually",
    )
    .await;
    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status = 'unknown', finished_at = NULL \
         WHERE id = $1",
    )
    .bind(action2)
    .execute(&f.pool)
    .await
    .expect("mark unknown");
    let result2: String = sqlx::query_scalar("SELECT viryaos_action_ledger_reconcile($1)")
        .bind(action2)
        .fetch_one(&f.pool)
        .await
        .expect("reconcile crash-marked");
    assert_eq!(
        result2, "UNKNOWN",
        "crash-marked failed must stay UNKNOWN — confirmation lost, NOT definitive failure"
    );

    // Fixture 3: definitive failure → FAILED
    let trace3 = uuid::Uuid::now_v7();
    let action3 = insert_decision_and_action(&f.pool, f.workspace_id, trace3).await;
    insert_community_post_with_error(
        &f.pool,
        f.workspace_id,
        action3,
        "failed",
        "r/t15c",
        "no agents service configured",
    )
    .await;
    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status = 'unknown', finished_at = NULL \
         WHERE id = $1",
    )
    .bind(action3)
    .execute(&f.pool)
    .await
    .expect("mark unknown");
    let result3: String = sqlx::query_scalar("SELECT viryaos_action_ledger_reconcile($1)")
        .bind(action3)
        .fetch_one(&f.pool)
        .await
        .expect("reconcile definitive failure");
    assert_eq!(
        result3, "FAILED",
        "definitive executor failure must reconcile to FAILED"
    );

    // Fixture 4: still posting → UNKNOWN
    let trace4 = uuid::Uuid::now_v7();
    let action4 = insert_decision_and_action(&f.pool, f.workspace_id, trace4).await;
    insert_community_post(&f.pool, f.workspace_id, action4, "posting", "r/t15d").await;
    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status = 'unknown', finished_at = NULL \
         WHERE id = $1",
    )
    .bind(action4)
    .execute(&f.pool)
    .await
    .expect("mark unknown");
    let result4: String = sqlx::query_scalar("SELECT viryaos_action_ledger_reconcile($1)")
        .bind(action4)
        .fetch_one(&f.pool)
        .await
        .expect("reconcile still posting");
    assert_eq!(
        result4, "UNKNOWN",
        "still-posting community_post must stay UNKNOWN"
    );
}

/// T16: Trace continuity invariant — no fake continuity.
///
/// An action created WITH a trace_id must propagate it to all downstream
/// records. An action created WITHOUT a trace_id (legacy) must NOT
/// fabricate a replacement trace — downstream records that claim
/// continuity must have NULL, not a fabricated UUID.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t16_trace_continuity_no_fake_continuity() {
    let f = setup().await.expect("fixture");

    // Case 1: action WITH trace_id → downstream records share it.
    let trace_id = uuid::Uuid::now_v7();
    let action_id = insert_decision_and_action(&f.pool, f.workspace_id, trace_id).await;

    // Insert a reach_event with the action's trace_id.
    sqlx::query(
        r#"INSERT INTO viryaos_reach_events
           (workspace_id, action_id, recipient_kind, recipient_id, channel,
            template_id, estimated_reach, status, trace_id)
           VALUES ($1, $2, 'subreddit_audience', 'r/t16', 'reddit_post',
                   'community-engager', 100, 'delivered', $3)"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(trace_id)
    .execute(&f.pool)
    .await
    .expect("insert reach event");

    let reach_trace: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT trace_id FROM viryaos_reach_events \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("reach trace");
    assert_eq!(
        reach_trace,
        Some(trace_id),
        "reach_event must have the action's trace_id — no fake continuity"
    );

    // Case 2: action WITHOUT trace_id (legacy) → downstream records have NULL.
    let decision_id = uuid::Uuid::now_v7();
    let legacy_action_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id)
           VALUES ($1,$2,$3,'growth_metrics','target_community',$4,
                   'auto_execute',9000,'auto_execute','test',
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,NULL)"#,
    )
    .bind(decision_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("decision-legacy-{decision_id}"))
    .bind(uuid::Uuid::now_v7())
    .execute(&f.pool)
    .await
    .expect("insert legacy decision");

    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, approved_at, available_at,
            finished_at, trace_id)
           VALUES ($1,$2,$3,'growth_metrics','community.engage.request','target_community',
                   $4,$5,$6,'succeeded',now(),now(),now(),NULL)"#,
    )
    .bind(legacy_action_id)
    .bind(f.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(uuid::Uuid::now_v7())
    .bind(format!("action-legacy-{legacy_action_id}"))
    .bind(serde_json::json!({"kind":"community.engage.request"}))
    .execute(&f.pool)
    .await
    .expect("insert legacy action");

    // Verify the legacy action has NULL trace_id.
    let legacy_trace: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT trace_id FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(legacy_action_id)
            .fetch_one(&f.pool)
            .await
            .expect("legacy trace");
    assert!(
        legacy_trace.is_none(),
        "legacy action must have NULL trace_id"
    );

    // Insert a reach_event for the legacy action with NULL trace_id
    // (simulating what the community executor should do).
    sqlx::query(
        r#"INSERT INTO viryaos_reach_events
           (workspace_id, action_id, recipient_kind, recipient_id, channel,
            template_id, estimated_reach, status, trace_id)
           VALUES ($1, $2, 'subreddit_audience', 'r/t16legacy', 'reddit_post',
                   'community-engager', 100, 'delivered', NULL)"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(legacy_action_id)
    .execute(&f.pool)
    .await
    .expect("insert legacy reach event");

    let legacy_reach_trace: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT trace_id FROM viryaos_reach_events \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(legacy_action_id)
    .fetch_one(&f.pool)
    .await
    .expect("legacy reach trace");
    assert!(
        legacy_reach_trace.is_none(),
        "legacy reach_event must have NULL trace_id — no fabricated continuity"
    );
}

/// T17: One growth episode per action.
///
/// The source-of-truth evidence cannot produce multiple conflicting
/// growth episodes for the same action. The UNIQUE(workspace_id, action_id)
/// constraint enforces this at the DB level, and the rebuild function
/// must preserve this invariant: rebuild(E) == rebuild(rebuild(E)).
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t17_one_episode_per_action() {
    let f = setup().await.expect("fixture");
    let trace_id = uuid::Uuid::now_v7();
    let action_id = insert_decision_and_action(&f.pool, f.workspace_id, trace_id).await;

    let design = f
        .repository
        .get_or_create_experiment_design(
            f.workspace_id,
            "community.engage",
            "cycle-t17",
            ExperimentUnitKind::TargetCommunity,
            vec!["r/t17treatment".to_string()],
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
        "r/t17treatment",
        "r/t17treatment",
        TreatmentAssignment::Treatment,
        &pred,
        Some(action_id),
    );
    f.repository
        .record_experiment_assignment(f.workspace_id, &assignment, Some("discovery"))
        .await
        .expect("treatment assignment recording");

    // Verify: exactly one episode exists.
    let episode_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_growth_episodes \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("episode count");
    assert_eq!(episode_count, 1, "exactly one episode per action");

    // Rebuild — must still produce exactly one episode (upsert, not insert).
    let rebuilt: i32 =
        sqlx::query_scalar("SELECT viryaos_rebuild_growth_episodes_from_evidence($1)")
            .bind(f.workspace_id.into_uuid())
            .fetch_one(&f.pool)
            .await
            .expect("rebuild 1");
    assert_eq!(rebuilt, 1, "rebuild must produce 1 episode");

    let episode_count_after_rebuild: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_growth_episodes \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("episode count after rebuild");
    assert_eq!(
        episode_count_after_rebuild, 1,
        "rebuild must not create duplicates — one episode per action"
    );

    // Rebuild again — idempotence.
    let rebuilt2: i32 =
        sqlx::query_scalar("SELECT viryaos_rebuild_growth_episodes_from_evidence($1)")
            .bind(f.workspace_id.into_uuid())
            .fetch_one(&f.pool)
            .await
            .expect("rebuild 2");
    assert_eq!(rebuilt2, 1, "second rebuild must also produce 1 episode");

    let episode_count_after_rebuild2: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_growth_episodes \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("episode count after rebuild 2");
    assert_eq!(
        episode_count_after_rebuild2, 1,
        "rebuild(rebuild(E)) must not create duplicates"
    );

    // Verify: growth_evidence.action_id is NOT NULL (invariant).
    let null_evidence: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_growth_evidence \
         WHERE workspace_id = $1 AND action_id IS NULL",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("null evidence check");
    assert_eq!(
        null_evidence, 0,
        "growth_evidence.action_id must never be NULL"
    );
}

/// T18: INSERT-vs-SELECT strictness.
///
/// Newly inserted rows may use safe construction defaults (known
/// constants). Existing persisted rows must be read strictly —
/// corrupt persisted metadata must produce an explicit error, not
/// a silent fallback.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t18_insert_vs_select_strictness() {
    let f = setup().await.expect("fixture");

    // INSERT path: newly inserted row → design constructed correctly.
    let design1 = f
        .repository
        .get_or_create_experiment_design(
            f.workspace_id,
            "community.engage",
            "cycle-t18-insert",
            ExperimentUnitKind::TargetCommunity,
            vec!["r/t18insert".to_string()],
            0.10,
            "discovery",
            10,
            2,
            2,
            f.now,
        )
        .await
        .expect("first creation must succeed");
    assert_eq!(
        design1.assignment_round, 1,
        "INSERT path: assignment_round must be 1 for new experiment"
    );

    // SELECT path (retry): same cycle key → must return the same design.
    let design2 = f
        .repository
        .get_or_create_experiment_design(
            f.workspace_id,
            "community.engage",
            "cycle-t18-insert",
            ExperimentUnitKind::TargetCommunity,
            vec!["r/t18insert".to_string()],
            0.10,
            "discovery",
            10,
            2,
            2,
            f.now,
        )
        .await
        .expect("retry must succeed");
    assert_eq!(
        design1.experiment_uuid, design2.experiment_uuid,
        "SELECT path: retry must return the same experiment_uuid"
    );

    // This used to inject `experiment_status = 'INVALID_STATUS'` and assert the
    // retry path errored rather than silently falling back to Active.
    // `viryaos_experiment_designs_experiment_status_check` was added since, and
    // now refuses the write — so the corrupt row cannot be built, and the test
    // was failing in its own setup rather than on the property it asserts.
    //
    // The guarantee did not weaken; it moved earlier and got stronger. An
    // invalid `experiment_status` is unrepresentable rather than merely
    // detected on read. Assert it at the boundary that now owns it.
    let corrupt_write = sqlx::query(
        "UPDATE viryaos_experiment_designs SET experiment_status = 'INVALID_STATUS' \
         WHERE workspace_id = $1 AND experiment_uuid = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(design1.experiment_uuid)
    .execute(&f.pool)
    .await;
    let error = corrupt_write.expect_err("an invalid experiment_status must be refused");
    assert!(
        matches!(&error, sqlx::Error::Database(db) if db.code().as_deref() == Some("23514")),
        "expected a check-constraint violation, got {error:?}"
    );

    // And the design is still readable and unchanged — the refused write left
    // nothing half-applied for the retry path to fall back on.
    let design3 = f
        .repository
        .get_or_create_experiment_design(
            f.workspace_id,
            "community.engage",
            "cycle-t18-insert",
            ExperimentUnitKind::TargetCommunity,
            vec!["r/t18insert".to_string()],
            0.10,
            "discovery",
            10,
            2,
            2,
            f.now,
        )
        .await
        .expect("retry after the refused corruption must still succeed");
    assert_eq!(
        design1.experiment_uuid, design3.experiment_uuid,
        "the refused write must not have changed the persisted design"
    );
}

/// T19: Full chain 3-branch proof.
///
/// Proves the complete semantic chain for all three execution outcomes:
///   SUCCESS → Dispatched → Executed → evidence → episode → trace → ledger
///   FAILURE → Dispatched → Failed → evidence → episode → trace → ledger
///   CONFIRMATION LOSS → Dispatched → Unknown → evidence → episode → trace → ledger
///
/// Critical invariant: Unknown ≠ Failed, Unknown ≠ Executed. No branch
/// accidentally teaches the brain the wrong thing.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t19_full_chain_three_branches() {
    let f = setup().await.expect("fixture");

    // Create one experiment design for all three branches.
    let design = f
        .repository
        .get_or_create_experiment_design(
            f.workspace_id,
            "community.engage",
            "cycle-t19",
            ExperimentUnitKind::TargetCommunity,
            vec![
                "r/t19success".to_string(),
                "r/t19failure".to_string(),
                "r/t19unknown".to_string(),
            ],
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

    // ── Branch 1: SUCCESS ──
    let trace_success = uuid::Uuid::now_v7();
    let action_success = insert_decision_and_action(&f.pool, f.workspace_id, trace_success).await;
    let assignment_success = ExperimentAssignment::from_design(
        &design,
        "r/t19success",
        "r/t19success",
        TreatmentAssignment::Treatment,
        &pred,
        Some(action_success),
    );
    f.repository
        .record_experiment_assignment(f.workspace_id, &assignment_success, Some("discovery"))
        .await
        .expect("success assignment");
    f.repository
        .update_execution_status_by_action_id(
            f.workspace_id,
            action_success,
            ExecutionStatus::Executed,
        )
        .await
        .expect("transition to executed");

    // ── Branch 2: FAILURE ──
    let trace_failure = uuid::Uuid::now_v7();
    let action_failure = insert_decision_and_action(&f.pool, f.workspace_id, trace_failure).await;
    let assignment_failure = ExperimentAssignment::from_design(
        &design,
        "r/t19failure",
        "r/t19failure",
        TreatmentAssignment::Treatment,
        &pred,
        Some(action_failure),
    );
    f.repository
        .record_experiment_assignment(f.workspace_id, &assignment_failure, Some("discovery"))
        .await
        .expect("failure assignment");
    f.repository
        .update_execution_status_by_action_id(
            f.workspace_id,
            action_failure,
            ExecutionStatus::Failed,
        )
        .await
        .expect("transition to failed");
    // `update_execution_status_by_action_id` writes the *assignment's* causal
    // realisation, which is an independent state machine — it does not touch
    // `viryaos_autopilot_actions.status`, and the ledger projects the action,
    // not the assignment. Without this the action kept the fixture's
    // `succeeded`, the ledger read SUCCEEDED, and the branch asserted FAILED
    // against a failure it had never actually recorded.
    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status = 'failed', finished_at = now() \
         WHERE id = $1",
    )
    .bind(action_failure)
    .execute(&f.pool)
    .await
    .expect("FAILURE branch: mark the action failed");

    // ── Branch 3: CONFIRMATION LOSS ──
    let trace_unknown = uuid::Uuid::now_v7();
    let action_unknown = insert_decision_and_action(&f.pool, f.workspace_id, trace_unknown).await;
    let assignment_unknown = ExperimentAssignment::from_design(
        &design,
        "r/t19unknown",
        "r/t19unknown",
        TreatmentAssignment::Treatment,
        &pred,
        Some(action_unknown),
    );
    f.repository
        .record_experiment_assignment(f.workspace_id, &assignment_unknown, Some("discovery"))
        .await
        .expect("unknown assignment");
    f.repository
        .update_execution_status_by_action_id(
            f.workspace_id,
            action_unknown,
            ExecutionStatus::Unknown,
        )
        .await
        .expect("transition to unknown");
    // Same as the FAILURE branch: the assignment's causal realisation is a
    // separate state machine, so the action must be moved for the ledger — its
    // projection — to read UNKNOWN. `finished_at` is cleared because confirmation
    // was lost, which is what the gap detector records.
    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status = 'unknown', finished_at = NULL \
         WHERE id = $1",
    )
    .bind(action_unknown)
    .execute(&f.pool)
    .await
    .expect("CONFIRMATION LOSS branch: mark the action unknown");

    // ── Verify: execution_status per branch ──
    let (success_arm, success_exec): (String, String) = sqlx::query_as(
        "SELECT arm::text, execution_status FROM viryaos_experiment_assignments \
         WHERE workspace_id = $1 AND unit_id = 'r/t19success'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("success query");
    assert_eq!(success_arm, "treatment");
    assert_eq!(success_exec, "executed", "SUCCESS branch: executed");

    let (failure_arm, failure_exec): (String, String) = sqlx::query_as(
        "SELECT arm::text, execution_status FROM viryaos_experiment_assignments \
         WHERE workspace_id = $1 AND unit_id = 'r/t19failure'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("failure query");
    assert_eq!(failure_arm, "treatment");
    assert_eq!(failure_exec, "failed", "FAILURE branch: failed");

    let (unknown_arm, unknown_exec): (String, String) = sqlx::query_as(
        "SELECT arm::text, execution_status FROM viryaos_experiment_assignments \
         WHERE workspace_id = $1 AND unit_id = 'r/t19unknown'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("unknown query");
    assert_eq!(unknown_arm, "treatment");
    assert_eq!(
        unknown_exec, "unknown",
        "CONFIRMATION LOSS branch: unknown — NOT failed, NOT executed"
    );

    // ── Verify: action ledger state per branch ──
    let success_ledger: String =
        sqlx::query_scalar("SELECT state FROM viryaos_action_ledger WHERE action_id = $1")
            .bind(action_success)
            .fetch_one(&f.pool)
            .await
            .expect("success ledger");
    assert_eq!(
        success_ledger, "SUCCEEDED",
        "SUCCESS branch: ledger SUCCEEDED"
    );

    let failure_ledger: String =
        sqlx::query_scalar("SELECT state FROM viryaos_action_ledger WHERE action_id = $1")
            .bind(action_failure)
            .fetch_one(&f.pool)
            .await
            .expect("failure ledger");
    assert_eq!(failure_ledger, "FAILED", "FAILURE branch: ledger FAILED");

    let unknown_ledger: String =
        sqlx::query_scalar("SELECT state FROM viryaos_action_ledger WHERE action_id = $1")
            .bind(action_unknown)
            .fetch_one(&f.pool)
            .await
            .expect("unknown ledger");
    assert_eq!(
        unknown_ledger, "UNKNOWN",
        "CONFIRMATION LOSS branch: ledger UNKNOWN — NOT FAILED"
    );

    // ── Verify: trace_id continuity per branch ──
    let success_trace: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT trace_id FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_success)
            .fetch_one(&f.pool)
            .await
            .expect("success trace");
    assert_eq!(
        success_trace,
        Some(trace_success),
        "SUCCESS branch: trace_id preserved"
    );

    let failure_trace: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT trace_id FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_failure)
            .fetch_one(&f.pool)
            .await
            .expect("failure trace");
    assert_eq!(
        failure_trace,
        Some(trace_failure),
        "FAILURE branch: trace_id preserved"
    );

    let unknown_trace: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT trace_id FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_unknown)
            .fetch_one(&f.pool)
            .await
            .expect("unknown trace");
    assert_eq!(
        unknown_trace,
        Some(trace_unknown),
        "CONFIRMATION LOSS branch: trace_id preserved"
    );

    // ── Verify: growth evidence exists per branch ──
    let evidence_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_growth_evidence \
         WHERE workspace_id = $1 AND action_id IN ($2, $3, $4)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_success)
    .bind(action_failure)
    .bind(action_unknown)
    .fetch_one(&f.pool)
    .await
    .expect("evidence count");
    assert_eq!(
        evidence_count, 3,
        "each branch must have exactly one growth evidence record"
    );

    // ── Verify: growth episodes exist per branch ──
    let episode_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_growth_episodes \
         WHERE workspace_id = $1 AND action_id IN ($2, $3, $4)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_success)
    .bind(action_failure)
    .bind(action_unknown)
    .fetch_one(&f.pool)
    .await
    .expect("episode count");
    assert_eq!(
        episode_count, 3,
        "each branch must have exactly one growth episode"
    );

    // ── Critical invariant: Unknown ≠ Failed, Unknown ≠ Executed ──
    // The causal learner must check execution_status, not just the arm.
    // Unknown is excluded from BOTH realized-treatment and failed-treatment
    // counts. This is the invariant that prevents the brain from learning
    // the wrong thing from confirmation loss.
    assert_ne!(
        unknown_exec, "failed",
        "CONFIRMATION LOSS must NOT be classified as failed treatment"
    );
    assert_ne!(
        unknown_exec, "executed",
        "CONFIRMATION LOSS must NOT be classified as realized treatment"
    );
    assert_ne!(
        unknown_ledger, "FAILED",
        "CONFIRMATION LOSS ledger must NOT be FAILED"
    );
}

/// T20: Causation_id propagation — the action's causation_id equals the
/// decision_id, and the outbox event's causation_id equals the action_id.
/// This proves the causal chain is written at every boundary.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t20_causation_id_propagation_across_boundaries() {
    let f = setup().await.expect("fixture");
    let trace_id = uuid::Uuid::now_v7();
    let decision_id = uuid::Uuid::now_v7();
    let action_id = uuid::Uuid::now_v7();

    // Insert decision (root — no causation_id)
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id)
           VALUES ($1,$2,$3,'growth_metrics','target_community',$4,
                   'auto_execute',9000,'auto_execute','test',
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)"#,
    )
    .bind(decision_id)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("decision-{decision_id}"))
    .bind(uuid::Uuid::now_v7())
    .bind(trace_id)
    .execute(&f.pool)
    .await
    .expect("insert decision");

    // Insert action with causation_id = decision_id
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, approved_at, available_at,
            finished_at, trace_id, causation_id)
           VALUES ($1,$2,$3,'growth_metrics','community.engage.request','target_community',
                   $4,$5,$6,'succeeded',now(),now(),now(),$7,$3)"#,
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(uuid::Uuid::now_v7())
    .bind(format!("action-{action_id}"))
    .bind(serde_json::json!({"kind":"community.engage.request"}))
    .bind(trace_id)
    .execute(&f.pool)
    .await
    .expect("insert action");

    // Insert outbox event with causation_id = action_id
    sqlx::query(
        r#"INSERT INTO outbox_events
           (workspace_id, event_type, event_version, payload, max_attempts,
            trace_id, causation_id, action_id)
           VALUES ($1, 'crowdrelay.autopilot.approval_requested', 1,
                   jsonb_build_object('action_id', $2, 'trace_id', $3),
                   12, $3, $2, $2)"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(trace_id)
    .execute(&f.pool)
    .await
    .expect("insert outbox event");

    // Verify: action.causation_id = decision_id
    let action_causation: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT causation_id FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("action causation");
    assert_eq!(
        action_causation,
        Some(decision_id),
        "action causation_id must equal decision_id"
    );

    // Verify: outbox.causation_id = action_id
    let outbox_causation: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT causation_id FROM outbox_events \
         WHERE workspace_id = $1 AND action_id = $2 LIMIT 1",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("outbox causation");
    assert_eq!(
        outbox_causation,
        Some(action_id),
        "outbox causation_id must equal action_id"
    );

    // Verify: ledger has causation_id propagated
    let ledger_causation: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT causation_id FROM viryaos_action_ledger WHERE action_id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("ledger causation");
    assert_eq!(
        ledger_causation,
        Some(decision_id),
        "ledger causation_id must equal the action's causation_id (decision_id)"
    );
}

/// T21: Ambiguous outbox outcome → action UNKNOWN.
///
/// When an outbox delivery exhausts retries with an ambiguous outcome
/// (transport timeout), the linked autopilot action must transition to
/// `unknown` — not `failed`. This is the core semantic guarantee: the
/// system does not lie about externally ambiguous side effects.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t21_ambiguous_outcome_transitions_action_to_unknown() {
    let f = setup().await.expect("fixture");
    let trace_id = uuid::Uuid::now_v7();
    let action_id = insert_decision_and_action(&f.pool, f.workspace_id, trace_id).await;

    // Insert an outbox event linked to the action, in 'dead' state with
    // an ambiguous error kind (transport_timeout).
    sqlx::query(
        r#"INSERT INTO outbox_events
           (workspace_id, event_type, event_version, payload, max_attempts,
            trace_id, action_id, status, last_error_kind, dead_at, attempts)
           VALUES ($1, 'crowdrelay.autopilot.approval_requested', 1,
                   jsonb_build_object('action_id', $2),
                   3, $3, $2, 'dead', 'transport_timeout', now(), 3)"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(trace_id)
    .execute(&f.pool)
    .await
    .expect("insert dead outbox event");

    // Manually transition the action to unknown (simulating what the
    // outbox worker does in finish_delivery).
    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status = 'unknown', updated_at = now() \
         WHERE id = $1 AND status IN ('succeeded', 'processing', 'queued', 'running')",
    )
    .bind(action_id)
    .execute(&f.pool)
    .await
    .expect("transition action to unknown");

    // Verify: action status is 'unknown'
    let action_status: String =
        sqlx::query_scalar("SELECT status FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("action status");
    assert_eq!(
        action_status, "unknown",
        "ambiguous outbox outcome must transition action to unknown, not failed"
    );

    // Verify: ledger state is UNKNOWN
    let ledger_state: String =
        sqlx::query_scalar("SELECT state FROM viryaos_action_ledger WHERE action_id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("ledger state");
    assert_eq!(
        ledger_state, "UNKNOWN",
        "ledger must reflect UNKNOWN for ambiguous outbox outcome"
    );
}

/// T22: Outbox reconciliation resolves UNKNOWN from delivered outbox.
///
/// An action in `unknown` state whose linked outbox event was eventually
/// delivered must be reconciled to `succeeded` by the outbox truth sweep.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t22_outbox_reconciliation_resolves_unknown_to_succeeded() {
    let f = setup().await.expect("fixture");
    let trace_id = uuid::Uuid::now_v7();
    // Not a community action: reconcile resolves this kind through the
    // outbox event's delivery status, which is what this test is about.
    let action_id = insert_decision_and_action_with_kind(
        &f.pool,
        f.workspace_id,
        trace_id,
        "signal.push.request",
    )
    .await;

    // Transition action to unknown first
    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status = 'unknown', updated_at = now() \
         WHERE id = $1 AND status = 'succeeded'",
    )
    .bind(action_id)
    .execute(&f.pool)
    .await
    .expect("transition to unknown");

    // Insert a delivered outbox event (external truth: the webhook was delivered)
    sqlx::query(
        r#"INSERT INTO outbox_events
           (workspace_id, event_type, event_version, payload, max_attempts,
            trace_id, action_id, status, delivered_at, attempts)
           VALUES ($1, 'crowdrelay.autopilot.approval_requested', 1,
                   jsonb_build_object('action_id', $2),
                   3, $3, $2, 'delivered', now(), 1)"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(trace_id)
    .execute(&f.pool)
    .await
    .expect("insert delivered outbox event");

    // Run the SQL reconciliation function
    let result: String = sqlx::query_scalar("SELECT viryaos_action_ledger_reconcile($1)")
        .bind(action_id)
        .fetch_one(&f.pool)
        .await
        .expect("reconcile");

    assert_eq!(
        result, "SUCCEEDED",
        "delivered outbox event must reconcile UNKNOWN → SUCCEEDED"
    );
}

/// T23: Outbox reconciliation stays UNKNOWN for ambiguous dead outbox.
///
/// An action in `unknown` state whose linked outbox event is dead with
/// an ambiguous error kind must stay `unknown` — only external truth
/// from the provider can resolve it.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t23_outbox_reconciliation_stays_unknown_for_ambiguous_dead() {
    let f = setup().await.expect("fixture");
    let trace_id = uuid::Uuid::now_v7();
    // Not a community action: reconcile resolves this kind through the
    // outbox event's delivery status, which is what this test is about.
    let action_id = insert_decision_and_action_with_kind(
        &f.pool,
        f.workspace_id,
        trace_id,
        "signal.push.request",
    )
    .await;

    // Transition action to unknown first
    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status = 'unknown', updated_at = now() \
         WHERE id = $1 AND status = 'succeeded'",
    )
    .bind(action_id)
    .execute(&f.pool)
    .await
    .expect("transition to unknown");

    // Insert a dead outbox event with ambiguous error kind
    sqlx::query(
        r#"INSERT INTO outbox_events
           (workspace_id, event_type, event_version, payload, max_attempts,
            trace_id, action_id, status, last_error_kind, dead_at, attempts)
           VALUES ($1, 'crowdrelay.autopilot.approval_requested', 1,
                   jsonb_build_object('action_id', $2),
                   3, $3, $2, 'dead', 'transport_timeout', now(), 3)"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(trace_id)
    .execute(&f.pool)
    .await
    .expect("insert dead outbox event");

    // Run the SQL reconciliation function
    let result: String = sqlx::query_scalar("SELECT viryaos_action_ledger_reconcile($1)")
        .bind(action_id)
        .fetch_one(&f.pool)
        .await
        .expect("reconcile");

    assert_eq!(
        result, "UNKNOWN",
        "ambiguous dead outbox event must stay UNKNOWN — not falsely resolved"
    );
}

/// T24: Outbox reconciliation resolves UNKNOWN to FAILED for permanent rejection.
///
/// An action in `unknown` state whose linked outbox event is dead with
/// a permanent rejection error kind must be reconciled to `failed`.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t24_outbox_reconciliation_resolves_unknown_to_failed_for_permanent() {
    let f = setup().await.expect("fixture");
    let trace_id = uuid::Uuid::now_v7();
    // Not a community action: reconcile resolves this kind through the
    // outbox event's delivery status, which is what this test is about.
    let action_id = insert_decision_and_action_with_kind(
        &f.pool,
        f.workspace_id,
        trace_id,
        "signal.push.request",
    )
    .await;

    // Transition action to unknown first
    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status = 'unknown', updated_at = now() \
         WHERE id = $1 AND status = 'succeeded'",
    )
    .bind(action_id)
    .execute(&f.pool)
    .await
    .expect("transition to unknown");

    // Insert a dead outbox event with permanent rejection error kind
    sqlx::query(
        r#"INSERT INTO outbox_events
           (workspace_id, event_type, event_version, payload, max_attempts,
            trace_id, action_id, status, last_error_kind, dead_at, attempts)
           VALUES ($1, 'crowdrelay.autopilot.approval_requested', 1,
                   jsonb_build_object('action_id', $2),
                   3, $3, $2, 'dead', 'http_permanent_status', now(), 3)"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .bind(trace_id)
    .execute(&f.pool)
    .await
    .expect("insert dead outbox event");

    // Run the SQL reconciliation function
    let result: String = sqlx::query_scalar("SELECT viryaos_action_ledger_reconcile($1)")
        .bind(action_id)
        .fetch_one(&f.pool)
        .await
        .expect("reconcile");

    assert_eq!(
        result, "FAILED",
        "permanent rejection must reconcile UNKNOWN → FAILED"
    );
}

// ── T25a–T25h: Atomic receipt resolution — UNKNOWN at receipt time ──
//
// These tests prove that `record_execution_report` resolves BOTH the
// action status and the experiment assignment execution_status atomically
// in the same transaction as receipt persistence. The reconciliation
// worker becomes a safety net for missing receipts, not part of the
// normal receipt-success path.

/// Helper: insert an action with an executor-required payload
/// (`fan.lifecycle.message.request`) and link an experiment assignment
/// to it. The action starts in `succeeded` status (the normal dispatch
/// state — `actions_execution.rs` marks it succeeded when dispatching).
async fn insert_executor_action_with_assignment(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    trace_id: uuid::Uuid,
    unit_id: &str,
    execution_status: &str,
) -> uuid::Uuid {
    let decision_id = uuid::Uuid::now_v7();
    let action_id = uuid::Uuid::now_v7();
    let fan_id = uuid::Uuid::now_v7();

    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id)
           VALUES ($1,$2,$3,'growth_metrics','target_community',$4,
                   'auto_execute',9000,'auto_execute','test',
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)"#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("decision-{decision_id}"))
    .bind(fan_id)
    .bind(trace_id)
    .execute(pool)
    .await
    .expect("insert decision");

    let payload = serde_json::json!({
        "kind": "request_fan_lifecycle_message",
        "fan_id": fan_id,
        "template_key": "test"
    });
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, approved_at, available_at,
            finished_at, trace_id)
           VALUES ($1,$2,$3,'growth_metrics','fan.lifecycle.message.request','fan',
                   $4,$5,$6,'succeeded',now(),now(),now(),$7)"#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(fan_id)
    .bind(format!("action-{action_id}"))
    .bind(&payload)
    .bind(trace_id)
    .execute(pool)
    .await
    .expect("insert action");

    // Insert an experiment assignment linked to this action.
    let assignment_id = uuid::Uuid::now_v7();
    let experiment_uuid = uuid::Uuid::now_v7();
    insert_experiment_design(pool, workspace_id, experiment_uuid).await;
    sqlx::query(
        r#"INSERT INTO viryaos_experiment_assignments
           (workspace_id, id, experiment_uuid, unit_id, unit_kind,
            arm, intended_template_id, propensity, prediction, context, strategy,
            eligibility_criteria, selection_context, interference_policy,
            contamination_estimate, is_interference_controllable,
            experiment_status, execution_status, action_id, trace_id)
           VALUES ($1,$2,$3,$4,'target_community','treatment','reddit-scanner',0.5,
                   '{}'::jsonb,'{}'::jsonb,'discovery',
                   '{}'::jsonb,'{}'::jsonb,'none',0.0,false,
                   'active',$5,$6,$7)"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(assignment_id)
    .bind(experiment_uuid)
    .bind(unit_id)
    .bind(execution_status)
    .bind(action_id)
    .bind(trace_id)
    .execute(pool)
    .await
    .expect("insert assignment");

    // An action can only be claimed once it has actually been emitted:
    // `claim_execution` requires an emission carrying an outbox event, because
    // claiming the execution of something never dispatched is meaningless.
    //
    // That guard was added after this fixture was written, so every claim-based
    // test here has been failing on `NotFound` ever since — reported honestly,
    // and unread because it was one of twenty-six failures.
    let outbox_event_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO outbox_events (id, workspace_id, event_type, payload)
           VALUES ($1, $2, 'test.action.emitted', '{}'::jsonb)"#,
    )
    .bind(outbox_event_id)
    .bind(workspace_id.into_uuid())
    .execute(pool)
    .await
    .expect("insert outbox event");
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_action_emissions
           (workspace_id, action_id, emission_key, outbox_event_id, emitted_at)
           VALUES ($1, $2, $3, $4, $5)"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(format!("test-emission:{action_id}"))
    .bind(outbox_event_id)
    .bind(OffsetDateTime::now_utc())
    .execute(pool)
    .await
    .expect("insert action emission");

    action_id
}

/// Helper: insert an executor instance + capability for the test workspace.
async fn insert_executor_instance(pool: &sqlx::PgPool, workspace_id: WorkspaceId) {
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        r#"INSERT INTO viryaos_executor_instances
           (workspace_id, executor_id, version, manifest_sha, observed_at, expires_at)
           VALUES ($1,'test-executor','test','test-manifest',$2,$3)"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(now + time::Duration::minutes(60))
    .execute(pool)
    .await
    .expect("insert executor instance");
    sqlx::query(
        r#"INSERT INTO viryaos_executor_capabilities
           (workspace_id, executor_id, capability, capability_version, observed_at, expires_at)
           VALUES ($1,'test-executor','fan.lifecycle.message','1',$2,$3)"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(now + time::Duration::minutes(60))
    .execute(pool)
    .await
    .expect("insert executor capability");
}

/// T25a: unknown + success receipt → action succeeded + assignment executed.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t25a_unknown_plus_success_receipt_resolves_both() {
    let f = setup().await.expect("fixture");
    insert_executor_instance(&f.pool, f.workspace_id).await;
    let trace_id = uuid::Uuid::now_v7();
    let action_id = insert_executor_action_with_assignment(
        &f.pool,
        f.workspace_id,
        trace_id,
        "r/t25a",
        "unknown",
    )
    .await;

    // Transition action to unknown (simulating gap detection).
    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status = 'unknown', finished_at = NULL, updated_at = now() \
         WHERE id = $1 AND status = 'succeeded'",
    )
    .bind(action_id)
    .execute(&f.pool)
    .await
    .expect("transition to unknown");

    // Claim the execution.
    let claim = f
        .repository
        .claim_execution(
            f.workspace_id,
            ClaimExecution {
                action_id: action_id.into(),
                executor_id: "test-executor".to_owned(),
                occurred_at: f.now,
            },
        )
        .await
        .expect("claim");
    let claim_token = claim.claim_token.expect("claim token");

    // Record a succeeded receipt.
    f.repository
        .record_execution_report(
            f.workspace_id,
            RecordExecutionReport {
                action_id: action_id.into(),
                receipt_key: format!("t25a-success-{action_id}"),
                executor_id: "test-executor".to_owned(),
                status: ExecutorReportStatus::Succeeded,
                claim_token: Some(claim_token),
                provider_reference: Some("msg-123".to_owned()),
                error_kind: None,
                metadata: serde_json::json!({}),
                occurred_at: f.now,
            },
        )
        .await
        .expect("record success receipt");

    // Verify: action resolved to succeeded.
    let action_status: String =
        sqlx::query_scalar("SELECT status FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("action status");
    assert_eq!(
        action_status, "succeeded",
        "unknown action must resolve to succeeded after success receipt"
    );

    // Verify: assignment resolved to executed.
    let exec_status: String = sqlx::query_scalar(
        "SELECT execution_status FROM viryaos_experiment_assignments WHERE action_id = $1",
    )
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("assignment status");
    assert_eq!(
        exec_status, "executed",
        "unknown assignment must resolve to executed after success receipt"
    );
}

/// T25b: unknown + failure receipt → action failed + assignment failed.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t25b_unknown_plus_failure_receipt_resolves_both() {
    let f = setup().await.expect("fixture");
    insert_executor_instance(&f.pool, f.workspace_id).await;
    let trace_id = uuid::Uuid::now_v7();
    let action_id = insert_executor_action_with_assignment(
        &f.pool,
        f.workspace_id,
        trace_id,
        "r/t25b",
        "unknown",
    )
    .await;

    // Transition action to unknown.
    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status = 'unknown', finished_at = NULL, updated_at = now() \
         WHERE id = $1 AND status = 'succeeded'",
    )
    .bind(action_id)
    .execute(&f.pool)
    .await
    .expect("transition to unknown");

    let claim = f
        .repository
        .claim_execution(
            f.workspace_id,
            ClaimExecution {
                action_id: action_id.into(),
                executor_id: "test-executor".to_owned(),
                occurred_at: f.now,
            },
        )
        .await
        .expect("claim");
    let claim_token = claim.claim_token.expect("claim token");

    f.repository
        .record_execution_report(
            f.workspace_id,
            RecordExecutionReport {
                action_id: action_id.into(),
                receipt_key: format!("t25b-fail-{action_id}"),
                executor_id: "test-executor".to_owned(),
                status: ExecutorReportStatus::Failed,
                claim_token: Some(claim_token),
                provider_reference: None,
                error_kind: Some("transport_failure".to_owned()),
                metadata: serde_json::json!({}),
                occurred_at: f.now,
            },
        )
        .await
        .expect("record failure receipt");

    let action_status: String =
        sqlx::query_scalar("SELECT status FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("action status");
    assert_eq!(action_status, "failed");

    let exec_status: String = sqlx::query_scalar(
        "SELECT execution_status FROM viryaos_experiment_assignments WHERE action_id = $1",
    )
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("assignment status");
    assert_eq!(exec_status, "failed");
}

/// T25c: duplicate success receipt is idempotent.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t25c_duplicate_success_receipt_is_idempotent() {
    let f = setup().await.expect("fixture");
    insert_executor_instance(&f.pool, f.workspace_id).await;
    let trace_id = uuid::Uuid::now_v7();
    let action_id = insert_executor_action_with_assignment(
        &f.pool,
        f.workspace_id,
        trace_id,
        "r/t25c",
        "dispatched",
    )
    .await;

    let claim = f
        .repository
        .claim_execution(
            f.workspace_id,
            ClaimExecution {
                action_id: action_id.into(),
                executor_id: "test-executor".to_owned(),
                occurred_at: f.now,
            },
        )
        .await
        .expect("claim");
    let claim_token = claim.claim_token.expect("claim token");

    let receipt_key = format!("t25c-success-{action_id}");
    let cmd = RecordExecutionReport {
        action_id: action_id.into(),
        receipt_key: receipt_key.clone(),
        executor_id: "test-executor".to_owned(),
        status: ExecutorReportStatus::Succeeded,
        claim_token: Some(claim_token),
        provider_reference: Some("msg-123".to_owned()),
        error_kind: None,
        metadata: serde_json::json!({}),
        occurred_at: f.now,
    };

    // First receipt.
    let first = f
        .repository
        .record_execution_report(f.workspace_id, cmd.clone())
        .await
        .expect("first receipt");
    assert!(!first.replayed);

    // Duplicate receipt — same receipt_key.
    let second = f
        .repository
        .record_execution_report(f.workspace_id, cmd)
        .await
        .expect("duplicate receipt");
    assert!(second.replayed, "duplicate receipt must be marked replayed");

    // Verify: no state regression.
    let action_status: String =
        sqlx::query_scalar("SELECT status FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("action status");
    assert_eq!(action_status, "succeeded");

    let exec_status: String = sqlx::query_scalar(
        "SELECT execution_status FROM viryaos_experiment_assignments WHERE action_id = $1",
    )
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("assignment status");
    assert_eq!(exec_status, "executed");

    // Verify: only one receipt row.
    let receipt_count: i64 =
        sqlx::query_scalar("SELECT count(*)::bigint FROM viryaos_autopilot_execution_reports WHERE workspace_id = $1 AND action_id = $2")
            .bind(f.workspace_id.into_uuid())
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("receipt count");
    assert_eq!(
        receipt_count, 1,
        "duplicate receipt must not create a second row"
    );
}

/// T25d: duplicate failure receipt is idempotent.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t25d_duplicate_failure_receipt_is_idempotent() {
    let f = setup().await.expect("fixture");
    insert_executor_instance(&f.pool, f.workspace_id).await;
    let trace_id = uuid::Uuid::now_v7();
    let action_id = insert_executor_action_with_assignment(
        &f.pool,
        f.workspace_id,
        trace_id,
        "r/t25d",
        "dispatched",
    )
    .await;

    let claim = f
        .repository
        .claim_execution(
            f.workspace_id,
            ClaimExecution {
                action_id: action_id.into(),
                executor_id: "test-executor".to_owned(),
                occurred_at: f.now,
            },
        )
        .await
        .expect("claim");
    let claim_token = claim.claim_token.expect("claim token");

    let receipt_key = format!("t25d-fail-{action_id}");
    let cmd = RecordExecutionReport {
        action_id: action_id.into(),
        receipt_key: receipt_key.clone(),
        executor_id: "test-executor".to_owned(),
        status: ExecutorReportStatus::Failed,
        claim_token: Some(claim_token),
        provider_reference: None,
        error_kind: Some("transport_failure".to_owned()),
        metadata: serde_json::json!({}),
        occurred_at: f.now,
    };

    let first = f
        .repository
        .record_execution_report(f.workspace_id, cmd.clone())
        .await
        .expect("first receipt");
    assert!(!first.replayed);

    let second = f
        .repository
        .record_execution_report(f.workspace_id, cmd)
        .await
        .expect("duplicate receipt");
    assert!(second.replayed);

    let action_status: String =
        sqlx::query_scalar("SELECT status FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("action status");
    assert_eq!(action_status, "failed");

    let exec_status: String = sqlx::query_scalar(
        "SELECT execution_status FROM viryaos_experiment_assignments WHERE action_id = $1",
    )
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("assignment status");
    assert_eq!(exec_status, "failed");
}

/// T25e: late success after unknown resolves correctly.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t25e_late_success_after_unknown_resolves_correctly() {
    let f = setup().await.expect("fixture");
    insert_executor_instance(&f.pool, f.workspace_id).await;
    let trace_id = uuid::Uuid::now_v7();
    let action_id = insert_executor_action_with_assignment(
        &f.pool,
        f.workspace_id,
        trace_id,
        "r/t25e",
        "unknown",
    )
    .await;

    // Simulate gap detection: action → unknown, assignment → unknown.
    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status = 'unknown', finished_at = NULL, updated_at = now() \
         WHERE id = $1 AND status = 'succeeded'",
    )
    .bind(action_id)
    .execute(&f.pool)
    .await
    .expect("transition action to unknown");

    let claim = f
        .repository
        .claim_execution(
            f.workspace_id,
            ClaimExecution {
                action_id: action_id.into(),
                executor_id: "test-executor".to_owned(),
                occurred_at: f.now,
            },
        )
        .await
        .expect("claim");
    let claim_token = claim.claim_token.expect("claim token");

    // Late success receipt arrives.
    f.repository
        .record_execution_report(
            f.workspace_id,
            RecordExecutionReport {
                action_id: action_id.into(),
                receipt_key: format!("t25e-late-success-{action_id}"),
                executor_id: "test-executor".to_owned(),
                status: ExecutorReportStatus::Succeeded,
                claim_token: Some(claim_token),
                provider_reference: Some("msg-late".to_owned()),
                error_kind: None,
                metadata: serde_json::json!({}),
                occurred_at: f.now,
            },
        )
        .await
        .expect("late success receipt");

    let action_status: String =
        sqlx::query_scalar("SELECT status FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("action status");
    assert_eq!(
        action_status, "succeeded",
        "late success must resolve unknown → succeeded"
    );

    let exec_status: String = sqlx::query_scalar(
        "SELECT execution_status FROM viryaos_experiment_assignments WHERE action_id = $1",
    )
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("assignment status");
    assert_eq!(
        exec_status, "executed",
        "late success must resolve unknown → executed"
    );
}

/// T25f: late failure after unknown resolves correctly.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t25f_late_failure_after_unknown_resolves_correctly() {
    let f = setup().await.expect("fixture");
    insert_executor_instance(&f.pool, f.workspace_id).await;
    let trace_id = uuid::Uuid::now_v7();
    let action_id = insert_executor_action_with_assignment(
        &f.pool,
        f.workspace_id,
        trace_id,
        "r/t25f",
        "unknown",
    )
    .await;

    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status = 'unknown', finished_at = NULL, updated_at = now() \
         WHERE id = $1 AND status = 'succeeded'",
    )
    .bind(action_id)
    .execute(&f.pool)
    .await
    .expect("transition action to unknown");

    let claim = f
        .repository
        .claim_execution(
            f.workspace_id,
            ClaimExecution {
                action_id: action_id.into(),
                executor_id: "test-executor".to_owned(),
                occurred_at: f.now,
            },
        )
        .await
        .expect("claim");
    let claim_token = claim.claim_token.expect("claim token");

    f.repository
        .record_execution_report(
            f.workspace_id,
            RecordExecutionReport {
                action_id: action_id.into(),
                receipt_key: format!("t25f-late-fail-{action_id}"),
                executor_id: "test-executor".to_owned(),
                status: ExecutorReportStatus::Failed,
                claim_token: Some(claim_token),
                provider_reference: None,
                error_kind: Some("late_transport_timeout".to_owned()),
                metadata: serde_json::json!({}),
                occurred_at: f.now,
            },
        )
        .await
        .expect("late failure receipt");

    let action_status: String =
        sqlx::query_scalar("SELECT status FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("action status");
    assert_eq!(
        action_status, "failed",
        "late failure must resolve unknown → failed"
    );

    let exec_status: String = sqlx::query_scalar(
        "SELECT execution_status FROM viryaos_experiment_assignments WHERE action_id = $1",
    )
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("assignment status");
    assert_eq!(
        exec_status, "failed",
        "late failure must resolve unknown → failed"
    );
}

/// T25g: resolved terminal state cannot regress (succeeded + late failure → no change).
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t25g_resolved_terminal_cannot_regress() {
    let f = setup().await.expect("fixture");
    insert_executor_instance(&f.pool, f.workspace_id).await;
    let trace_id = uuid::Uuid::now_v7();
    let action_id = insert_executor_action_with_assignment(
        &f.pool,
        f.workspace_id,
        trace_id,
        "r/t25g",
        "dispatched",
    )
    .await;

    let claim = f
        .repository
        .claim_execution(
            f.workspace_id,
            ClaimExecution {
                action_id: action_id.into(),
                executor_id: "test-executor".to_owned(),
                occurred_at: f.now,
            },
        )
        .await
        .expect("claim");
    let claim_token = claim.claim_token.expect("claim token");

    // First: success receipt → action succeeded, assignment executed.
    f.repository
        .record_execution_report(
            f.workspace_id,
            RecordExecutionReport {
                action_id: action_id.into(),
                receipt_key: format!("t25g-success-{action_id}"),
                executor_id: "test-executor".to_owned(),
                status: ExecutorReportStatus::Succeeded,
                claim_token: Some(claim_token),
                provider_reference: Some("msg-1".to_owned()),
                error_kind: None,
                metadata: serde_json::json!({}),
                occurred_at: f.now,
            },
        )
        .await
        .expect("success receipt");

    // Verify state after success.
    let (action_status, exec_status): (String, String) = sqlx::query_as(
        "SELECT a.status, ea.execution_status \
         FROM viryaos_autopilot_actions a \
         JOIN viryaos_experiment_assignments ea ON ea.action_id = a.id \
         WHERE a.id = $1",
    )
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("state after success");
    assert_eq!(action_status, "succeeded");
    assert_eq!(exec_status, "executed");

    // Second: late failure receipt (different receipt_key, same executor).
    // The provider_already_succeeded check must prevent regression.
    f.repository
        .record_execution_report(
            f.workspace_id,
            RecordExecutionReport {
                action_id: action_id.into(),
                receipt_key: format!("t25g-late-fail-{action_id}"),
                executor_id: "test-executor".to_owned(),
                status: ExecutorReportStatus::Failed,
                claim_token: Some(claim_token),
                provider_reference: None,
                error_kind: Some("late_timeout".to_owned()),
                metadata: serde_json::json!({}),
                occurred_at: f.now + time::Duration::minutes(5),
            },
        )
        .await
        .expect("late failure receipt");

    // Verify: state must NOT regress.
    let (action_status_after, exec_status_after): (String, String) = sqlx::query_as(
        "SELECT a.status, ea.execution_status \
         FROM viryaos_autopilot_actions a \
         JOIN viryaos_experiment_assignments ea ON ea.action_id = a.id \
         WHERE a.id = $1",
    )
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("state after late failure");
    assert_eq!(
        action_status_after, "succeeded",
        "action must not regress from succeeded to failed"
    );
    assert_eq!(
        exec_status_after, "executed",
        "assignment must not regress from executed to failed"
    );
}

/// T25h: no split-brain state after commit (action and assignment agree).
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t25h_no_split_brain_after_commit() {
    let f = setup().await.expect("fixture");
    insert_executor_instance(&f.pool, f.workspace_id).await;

    // Test both success and failure paths — in both cases, action and
    // assignment must agree after commit.
    for (unit_id, status, expected_action, expected_assignment) in [
        (
            "r/t25h-success",
            ExecutorReportStatus::Succeeded,
            "succeeded",
            "executed",
        ),
        (
            "r/t25h-failure",
            ExecutorReportStatus::Failed,
            "failed",
            "failed",
        ),
    ] {
        let trace_id = uuid::Uuid::now_v7();
        let action_id = insert_executor_action_with_assignment(
            &f.pool,
            f.workspace_id,
            trace_id,
            unit_id,
            "unknown",
        )
        .await;

        // Transition action to unknown.
        sqlx::query(
            "UPDATE viryaos_autopilot_actions SET status = 'unknown', finished_at = NULL, updated_at = now() \
             WHERE id = $1 AND status = 'succeeded'",
        )
        .bind(action_id)
        .execute(&f.pool)
        .await
        .expect("transition to unknown");

        let claim = f
            .repository
            .claim_execution(
                f.workspace_id,
                ClaimExecution {
                    action_id: action_id.into(),
                    executor_id: "test-executor".to_owned(),
                    occurred_at: f.now,
                },
            )
            .await
            .expect("claim");
        let claim_token = claim.claim_token.expect("claim token");

        f.repository
            .record_execution_report(
                f.workspace_id,
                RecordExecutionReport {
                    action_id: action_id.into(),
                    receipt_key: format!("t25h-{unit_id}-{action_id}"),
                    executor_id: "test-executor".to_owned(),
                    status,
                    claim_token: Some(claim_token),
                    provider_reference: if matches!(status, ExecutorReportStatus::Succeeded) {
                        Some("msg".to_owned())
                    } else {
                        None
                    },
                    error_kind: if matches!(status, ExecutorReportStatus::Failed) {
                        Some("error".to_owned())
                    } else {
                        None
                    },
                    metadata: serde_json::json!({}),
                    occurred_at: f.now,
                },
            )
            .await
            .expect("receipt");

        // Verify: action and assignment agree.
        let (action_status, exec_status): (String, String) = sqlx::query_as(
            "SELECT a.status, ea.execution_status \
             FROM viryaos_autopilot_actions a \
             JOIN viryaos_experiment_assignments ea ON ea.action_id = a.id \
             WHERE a.id = $1",
        )
        .bind(action_id)
        .fetch_one(&f.pool)
        .await
        .expect("state query");
        assert_eq!(
            action_status, expected_action,
            "action status must be {expected_action} for {unit_id}"
        );
        assert_eq!(
            exec_status, expected_assignment,
            "assignment status must be {expected_assignment} for {unit_id}"
        );
        // No split-brain: action and assignment must both be terminal.
        assert!(
            action_status != "unknown",
            "action must not remain unknown after terminal receipt"
        );
        assert!(
            exec_status != "unknown" && exec_status != "dispatched",
            "assignment must not remain unknown/dispatched after terminal receipt"
        );
    }
}

// ── T25i: Learning boundary — UNKNOWN excluded from causal learner ──
//
// Proves that the explicit LEFT JOIN guard in load_growth_evidence
// excludes execution_status='unknown' evidence from the causal learner,
// even if resolved_at is set (simulating a bug/admin override).

/// Helper: insert a growth evidence row directly.
async fn insert_growth_evidence_row(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    action_id: uuid::Uuid,
    resolved: bool,
) {
    sqlx::query(
        r#"INSERT INTO viryaos_growth_evidence
           (workspace_id, action_id, opportunity_id, timestamp, audience,
            recipient_id, channel, estimated_reach, treatment, propensity,
            observed_fans, observed_incremental_fans, durable_fans_30d,
            converted, predicted_fans, predicted_signal_installs, context,
            strategy, evidence_quality, resolved_at)
           VALUES ($1,$2,$3,now(),'test','test_recipient','reddit_post',1,
                   'treatment',0.5,10.0,5.0,3.0,false,5.0,1.0,
                   '{}'::jsonb,'discovery','observational',$4)"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(format!("opp-{action_id}"))
    .bind(if resolved {
        Some(OffsetDateTime::now_utc())
    } else {
        None
    })
    .execute(pool)
    .await
    .expect("insert growth evidence");
}

/// Helper: insert an experiment assignment with a specific execution_status.
async fn insert_assignment_for_evidence_test(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    action_id: uuid::Uuid,
    unit_id: &str,
    execution_status: &str,
) {
    let assignment_id = uuid::Uuid::now_v7();
    let experiment_uuid = uuid::Uuid::now_v7();
    insert_experiment_design(pool, workspace_id, experiment_uuid).await;
    sqlx::query(
        r#"INSERT INTO viryaos_experiment_assignments
           (workspace_id, id, experiment_uuid, unit_id, unit_kind,
            arm, intended_template_id, propensity, prediction, context, strategy,
            eligibility_criteria, selection_context, interference_policy,
            contamination_estimate, is_interference_controllable,
            experiment_status, execution_status, action_id)
           VALUES ($1,$2,$3,$4,'target_community','treatment','reddit-scanner',0.5,
                   '{}'::jsonb,'{}'::jsonb,'discovery',
                   '{}'::jsonb,'{}'::jsonb,'none',0.0,false,
                   'active',$5,$6)"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(assignment_id)
    .bind(experiment_uuid)
    .bind(unit_id)
    .bind(execution_status)
    .bind(action_id)
    .execute(pool)
    .await
    .expect("insert assignment");

    // Same guard `insert_executor_action_with_assignment` already documents:
    // `record_execution_report` inserts its receipt only
    // `WHERE EXISTS (SELECT 1 FROM viryaos_autopilot_action_emissions ...)`,
    // because reporting the outcome of something never dispatched is
    // meaningless. This helper was never given an emission, so every test that
    // reports an outcome through it failed with `NotFound` — the repository was
    // right and the fixture was incomplete.
    let outbox_event_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO outbox_events (id, workspace_id, event_type, payload)
           VALUES ($1, $2, 'test.action.emitted', '{}'::jsonb)"#,
    )
    .bind(outbox_event_id)
    .bind(workspace_id.into_uuid())
    .execute(pool)
    .await
    .expect("insert outbox event");
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_action_emissions
           (workspace_id, action_id, emission_key, outbox_event_id, emitted_at)
           VALUES ($1, $2, $3, $4, $5)"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(format!("test-evidence-emission:{action_id}"))
    .bind(outbox_event_id)
    .bind(OffsetDateTime::now_utc())
    .execute(pool)
    .await
    .expect("insert action emission");
}

/// T25i: UNKNOWN evidence excluded from causal learner even with resolved_at set.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t25i_unknown_excluded_from_causal_learner() {
    let f = setup().await.expect("fixture");

    // 1. Create an action + assignment with execution_status=unknown.
    let trace_unknown = uuid::Uuid::now_v7();
    let action_unknown = insert_decision_and_action(&f.pool, f.workspace_id, trace_unknown).await;
    insert_assignment_for_evidence_test(
        &f.pool,
        f.workspace_id,
        action_unknown,
        "r/t25i-unknown",
        "unknown",
    )
    .await;
    // Deliberately set resolved_at despite the invalid state (simulating
    // a bug/admin override that sets resolved_at despite unknown status).
    insert_growth_evidence_row(&f.pool, f.workspace_id, action_unknown, true).await;

    // 2. Create an action + assignment with execution_status=executed.
    let trace_executed = uuid::Uuid::now_v7();
    let action_executed = insert_decision_and_action(&f.pool, f.workspace_id, trace_executed).await;
    insert_assignment_for_evidence_test(
        &f.pool,
        f.workspace_id,
        action_executed,
        "r/t25i-executed",
        "executed",
    )
    .await;
    insert_growth_evidence_row(&f.pool, f.workspace_id, action_executed, true).await;

    // 3. Create an action + assignment with execution_status=failed.
    let trace_failed = uuid::Uuid::now_v7();
    let action_failed = insert_decision_and_action(&f.pool, f.workspace_id, trace_failed).await;
    insert_assignment_for_evidence_test(
        &f.pool,
        f.workspace_id,
        action_failed,
        "r/t25i-failed",
        "failed",
    )
    .await;
    insert_growth_evidence_row(&f.pool, f.workspace_id, action_failed, true).await;

    // 4. Create an action with NO assignment (non-experiment evidence).
    let trace_no_assign = uuid::Uuid::now_v7();
    let action_no_assign =
        insert_decision_and_action(&f.pool, f.workspace_id, trace_no_assign).await;
    insert_growth_evidence_row(&f.pool, f.workspace_id, action_no_assign, true).await;

    // 5. Load the causal-learning dataset.
    let evidence = f
        .repository
        .load_growth_evidence(f.workspace_id, None)
        .await
        .expect("load growth evidence");

    // 6. Assert: unknown evidence is excluded.
    let unknown_loaded = evidence.iter().any(|e| e.action_id == Some(action_unknown));
    assert!(
        !unknown_loaded,
        "unknown execution_status evidence must be excluded from the causal learner"
    );

    // 7. Assert: executed evidence is included.
    let executed_loaded = evidence
        .iter()
        .any(|e| e.action_id == Some(action_executed));
    assert!(
        executed_loaded,
        "executed execution_status evidence must be included in the causal learner"
    );

    // 8. Assert: failed evidence is included (valid non-treatment evidence).
    let failed_loaded = evidence.iter().any(|e| e.action_id == Some(action_failed));
    assert!(
        failed_loaded,
        "failed execution_status evidence must be included (valid non-treatment for per-protocol)"
    );

    // 9. Assert: non-experiment evidence (no assignment) is included.
    let no_assign_loaded = evidence
        .iter()
        .any(|e| e.action_id == Some(action_no_assign));
    assert!(
        no_assign_loaded,
        "non-experiment evidence (no assignment) must be included — LEFT JOIN, not INNER"
    );

    // 10. Assert: no fan-out — each action_id appears at most once.
    let mut action_ids: Vec<_> = evidence.iter().filter_map(|e| e.action_id).collect();
    let total = action_ids.len();
    action_ids.sort();
    action_ids.dedup();
    assert_eq!(
        action_ids.len(),
        total,
        "no fan-out: one evidence record must produce at most one learning row"
    );
}

// ── T26: Concurrent resolution race ──
//
// Two workers attempt to resolve the same UNKNOWN action simultaneously.
// The FOR UPDATE lock + WHERE status = 'unknown' guard must ensure
// exactly one worker transitions the action; the other sees it already
// resolved and does nothing. No duplicate effects, no split-brain.

/// T26: Concurrent resolution race — two workers, one action, one winner.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t26_concurrent_resolution_race_one_winner() {
    let f = setup().await.expect("fixture");
    let trace_id = uuid::Uuid::now_v7();
    let action_id = insert_decision_and_action(&f.pool, f.workspace_id, trace_id).await;

    // Set the action to 'unknown' (simulating gap detection).
    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status = 'unknown', finished_at = NULL \
         WHERE id = $1 AND workspace_id = $2",
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .execute(&f.pool)
    .await
    .expect("set unknown");

    // Insert an assignment in 'dispatched' state.
    insert_assignment_for_evidence_test(
        &f.pool,
        f.workspace_id,
        action_id,
        "r/t26-race",
        "dispatched",
    )
    .await;

    // Two concurrent success receipts for the same action.
    let repo = f.repository.clone();
    let ws = f.workspace_id;
    let now = OffsetDateTime::now_utc();
    let make_cmd = || RecordExecutionReport {
        action_id: action_id.into(),
        receipt_key: format!("t26-success-{action_id}"),
        executor_id: "test-executor".to_owned(),
        status: ExecutorReportStatus::Succeeded,
        claim_token: None,
        provider_reference: Some("msg-t26".to_owned()),
        error_kind: None,
        metadata: serde_json::json!({}),
        occurred_at: now,
    };

    let (r1, r2) = tokio::join!(
        repo.record_execution_report(ws, make_cmd()),
        repo.record_execution_report(ws, make_cmd()),
    );

    // Both should succeed (no error) — the WHERE guard makes the loser
    // a no-op. Neither should panic or corrupt state.
    assert!(r1.is_ok(), "first worker should not error: {:?}", r1);
    assert!(r2.is_ok(), "second worker should not error: {:?}", r2);

    // Verify: action is succeeded (not duplicated, not corrupted).
    let final_status: String =
        sqlx::query_scalar("SELECT status FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("final status");
    assert_eq!(
        final_status, "succeeded",
        "action must be succeeded after concurrent resolution"
    );

    // Verify: exactly one assignment transition (dispatched → executed).
    let exec_status: String = sqlx::query_scalar(
        "SELECT execution_status FROM viryaos_experiment_assignments \
                            WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("assignment status");
    assert_eq!(
        exec_status, "executed",
        "assignment must be executed — exactly one transition"
    );

    // Verify: one receipt, one audit row.
    //
    // Both workers here submit the *same* `receipt_key` — `make_cmd()` builds
    // it from `action_id` alone — so this is one receipt delivered twice, not
    // two observations. `record_execution_report` inserts
    // `ON CONFLICT (workspace_id, receipt_key) DO NOTHING`, which is the same
    // dedup t25d depends on for `second.replayed == true`.
    //
    // This asserted 2 and could never have passed: expecting two audit rows
    // for one receipt key contradicts the idempotency contract the suite
    // relies on elsewhere. Two genuine observations need two receipt keys.
    let report_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_autopilot_execution_reports \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("report count");
    assert_eq!(
        report_count, 1,
        "one receipt key is one receipt — the racing duplicate is deduped, \
         and exactly one transition happened"
    );
}

// ── T27: Contradictory provider facts ──
//
// Provider says "posted" (Confirmed) while the action is already Failed.
// The resolver must surface this as Conflict, not silently revive the
// action or downgrade the failure. The state must NOT change.

/// T27: Contradictory provider facts — success receipt for a Failed action → Conflict, no state change.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t27_contradictory_provider_facts_no_state_change() {
    let f = setup().await.expect("fixture");
    let trace_id = uuid::Uuid::now_v7();
    let action_id = insert_decision_and_action(&f.pool, f.workspace_id, trace_id).await;

    // Set the action to 'failed' (definitive failure).
    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status = 'failed', finished_at = now() \
         WHERE id = $1 AND workspace_id = $2",
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .execute(&f.pool)
    .await
    .expect("set failed");

    // Insert assignment in 'failed' state.
    insert_assignment_for_evidence_test(
        &f.pool,
        f.workspace_id,
        action_id,
        "r/t27-contradiction",
        "failed",
    )
    .await;

    // Now a success receipt arrives — contradicting the failed state.
    let cmd = RecordExecutionReport {
        action_id: action_id.into(),
        receipt_key: format!("t27-success-{action_id}"),
        executor_id: "test-executor".to_owned(),
        status: ExecutorReportStatus::Succeeded,
        claim_token: None,
        provider_reference: Some("msg-t27".to_owned()),
        error_kind: None,
        metadata: serde_json::json!({}),
        occurred_at: OffsetDateTime::now_utc(),
    };

    // This should NOT error — the resolver surfaces Conflict and returns
    // early without changing state.
    let result = f
        .repository
        .record_execution_report(f.workspace_id, cmd)
        .await;
    assert!(
        result.is_ok(),
        "contradictory receipt should not error — it surfaces Conflict: {:?}",
        result
    );

    // Verify: action is STILL failed — not revived.
    let final_status: String =
        sqlx::query_scalar("SELECT status FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("final status");
    assert_eq!(
        final_status, "failed",
        "contradictory success must NOT revive a failed action"
    );

    // Verify: assignment is STILL failed — not revived.
    let exec_status: String = sqlx::query_scalar(
        "SELECT execution_status FROM viryaos_experiment_assignments \
                            WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("assignment status");
    assert_eq!(
        exec_status, "failed",
        "contradictory success must NOT revive a failed assignment"
    );
}

// ── North Star Test A: success → lost → UNKNOWN → recovery → exactly one effect ──
//
// The full lifecycle:
// 1. Action dispatched (assignment = dispatched)
// 2. Executor reports success (action = succeeded, assignment = executed)
// 3. Confirmation lost (community executor crash → action = unknown, assignment = unknown)
// 4. Reconciliation discovers the post was actually posted → action = succeeded, assignment = executed
// 5. Assert: exactly one action, one assignment, one evidence, one episode
// 6. Assert: no double execution, no duplicate evidence, no causal update during UNKNOWN

/// North Star A: success → confirmation lost → UNKNOWN → recovery → exactly one effect.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn north_star_a_success_lost_unknown_recovery_one_effect() {
    let f = setup().await.expect("fixture");
    let trace_id = uuid::Uuid::now_v7();
    let action_id = insert_decision_and_action(&f.pool, f.workspace_id, trace_id).await;

    // 1. Insert assignment in 'dispatched' state.
    insert_assignment_for_evidence_test(
        &f.pool,
        f.workspace_id,
        action_id,
        "r/north-star-a",
        "dispatched",
    )
    .await;

    // 2. Executor reports success → action = succeeded, assignment = executed.
    f.repository
        .record_execution_report(
            f.workspace_id,
            RecordExecutionReport {
                action_id: action_id.into(),
                receipt_key: format!("ns-a-success-{action_id}"),
                executor_id: "test-executor".to_owned(),
                status: ExecutorReportStatus::Succeeded,
                claim_token: None,
                provider_reference: Some("msg-ns-a".to_owned()),
                error_kind: None,
                metadata: serde_json::json!({}),
                occurred_at: OffsetDateTime::now_utc(),
            },
        )
        .await
        .expect("success report");

    let status_after_success: String =
        sqlx::query_scalar("SELECT status FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("status after success");
    assert_eq!(status_after_success, "succeeded");

    // 3. Confirmation lost — community executor crash marks it unknown.
    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status = 'unknown', finished_at = NULL \
         WHERE id = $1 AND workspace_id = $2 AND status IN ('succeeded', 'processing')",
    )
    .bind(action_id)
    .bind(f.workspace_id.into_uuid())
    .execute(&f.pool)
    .await
    .expect("confirmation lost");

    sqlx::query(
        "UPDATE viryaos_experiment_assignments SET execution_status = 'unknown' \
         WHERE workspace_id = $1 AND action_id = $2 AND execution_status = 'executed'",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .execute(&f.pool)
    .await
    .expect("assignment to unknown");

    let status_after_loss: String =
        sqlx::query_scalar("SELECT status FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("status after loss");
    assert_eq!(status_after_loss, "unknown");

    // 4. Reconciliation discovers the post was actually posted.
    //    Insert a community_posts row with status='posted'.
    insert_community_post(
        &f.pool,
        f.workspace_id,
        action_id,
        "posted",
        "r/north-star-a",
    )
    .await;

    // Run the SQL reconciliation function (same as T15 uses).
    let result: String = sqlx::query_scalar("SELECT viryaos_action_ledger_reconcile($1)")
        .bind(action_id)
        .fetch_one(&f.pool)
        .await
        .expect("reconcile");
    assert_eq!(
        result, "SUCCEEDED",
        "reconciliation must recover the action to SUCCEEDED"
    );

    // 5. Assert: action recovered to succeeded.
    let status_after_recovery: String =
        sqlx::query_scalar("SELECT status FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("status after recovery");
    assert_eq!(
        status_after_recovery, "succeeded",
        "action must recover to succeeded after reconciliation"
    );

    // 6. Assert: exactly one evidence record (no duplication during loss/recovery).
    let evidence_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_growth_evidence \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("evidence count");
    assert_eq!(
        evidence_count, 1,
        "exactly one evidence record — no duplication during loss/recovery"
    );

    // 7. Assert: exactly one episode.
    let episode_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_growth_episodes \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("episode count");
    assert_eq!(episode_count, 1, "exactly one episode — no duplication");

    // 8. Assert: trace_id preserved throughout.
    let final_trace: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT trace_id FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&f.pool)
            .await
            .expect("trace");
    assert_eq!(
        final_trace,
        Some(trace_id),
        "trace_id must be preserved through loss and recovery"
    );
}

// ── North Star Test B: UNKNOWN → definitive proof DID NOT happen → FAILED → safe retry ──
//
// The full lifecycle:
// 1. Action dispatched (assignment = dispatched)
// 2. Confirmation lost (action = unknown, assignment = unknown)
// 3. Definitive evidence that the original intervention did NOT happen
//    (community_posts.status = 'failed' with non-crash error)
// 4. Reconciliation resolves UNKNOWN → FAILED
// 5. Assert: action is failed, assignment is failed
// 6. Assert: retry is permitted (a new action can be created)
// 7. Assert: the original action stays failed (not revived by the retry)
// 8. Assert: exactly one eventual real intervention (the retry succeeds)

/// North Star B: UNKNOWN → definitive non-execution → FAILED → safe retry → one effect.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn north_star_b_unknown_definitive_failure_safe_retry_one_effect() {
    let f = setup().await.expect("fixture");
    let trace_id_1 = uuid::Uuid::now_v7();
    let action_id_1 = insert_decision_and_action(&f.pool, f.workspace_id, trace_id_1).await;

    // 1. Insert assignment in 'dispatched' state.
    insert_assignment_for_evidence_test(
        &f.pool,
        f.workspace_id,
        action_id_1,
        "r/north-star-b-original",
        "dispatched",
    )
    .await;

    // 2. Confirmation lost — action goes to unknown.
    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status = 'unknown', finished_at = NULL \
         WHERE id = $1 AND workspace_id = $2",
    )
    .bind(action_id_1)
    .bind(f.workspace_id.into_uuid())
    .execute(&f.pool)
    .await
    .expect("set unknown");

    sqlx::query(
        "UPDATE viryaos_experiment_assignments SET execution_status = 'unknown' \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id_1)
    .execute(&f.pool)
    .await
    .expect("assignment to unknown");

    // 3. Definitive evidence that the original did NOT happen —
    //    community_posts.status = 'failed' with a non-crash error message.
    insert_community_post(
        &f.pool,
        f.workspace_id,
        action_id_1,
        "failed",
        "r/north-star-b-original",
    )
    .await;
    // Set a non-crash error message to ensure it's treated as DefinitiveFailure.
    sqlx::query(
        "UPDATE community_posts SET error_message = 'no agents service configured' \
                 WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id_1)
    .execute(&f.pool)
    .await
    .expect("set error message");

    // 4. Reconciliation resolves UNKNOWN → FAILED via SQL function.
    let result: String = sqlx::query_scalar("SELECT viryaos_action_ledger_reconcile($1)")
        .bind(action_id_1)
        .fetch_one(&f.pool)
        .await
        .expect("reconcile");
    assert_eq!(
        result, "FAILED",
        "reconciliation must resolve the action to FAILED"
    );

    // 5. Assert: action is failed.
    let status_after_fail: String =
        sqlx::query_scalar("SELECT status FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_id_1)
            .fetch_one(&f.pool)
            .await
            .expect("status after fail");
    assert_eq!(
        status_after_fail, "failed",
        "original action must be failed after definitive non-execution"
    );

    // 6. Retry: create a NEW action for the same opportunity.
    let trace_id_2 = uuid::Uuid::now_v7();
    let action_id_2 = insert_decision_and_action(&f.pool, f.workspace_id, trace_id_2).await;
    insert_assignment_for_evidence_test(
        &f.pool,
        f.workspace_id,
        action_id_2,
        "r/north-star-b-retry",
        "dispatched",
    )
    .await;

    // The retry succeeds.
    f.repository
        .record_execution_report(
            f.workspace_id,
            RecordExecutionReport {
                action_id: action_id_2.into(),
                receipt_key: format!("ns-b-retry-{action_id_2}"),
                executor_id: "test-executor".to_owned(),
                status: ExecutorReportStatus::Succeeded,
                claim_token: None,
                provider_reference: Some("msg-ns-b-retry".to_owned()),
                error_kind: None,
                metadata: serde_json::json!({}),
                occurred_at: OffsetDateTime::now_utc(),
            },
        )
        .await
        .expect("retry success");

    // 7. Assert: original action is STILL failed (not revived by retry).
    let original_status: String =
        sqlx::query_scalar("SELECT status FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_id_1)
            .fetch_one(&f.pool)
            .await
            .expect("original status");
    assert_eq!(
        original_status, "failed",
        "original action must stay failed — retry must not revive it"
    );

    // 8. Assert: retry action is succeeded.
    let retry_status: String =
        sqlx::query_scalar("SELECT status FROM viryaos_autopilot_actions WHERE id = $1")
            .bind(action_id_2)
            .fetch_one(&f.pool)
            .await
            .expect("retry status");
    assert_eq!(retry_status, "succeeded", "retry action must be succeeded");

    // 9. Assert: two evidence records (one per action), no duplication.
    let evidence_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_growth_evidence \
         WHERE workspace_id = $1 AND action_id IN ($2, $3)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id_1)
    .bind(action_id_2)
    .fetch_one(&f.pool)
    .await
    .expect("evidence count");
    assert_eq!(
        evidence_count, 2,
        "exactly two evidence records — one per action, no duplication"
    );

    // 10. Assert: two episodes (one per action).
    let episode_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_growth_episodes \
         WHERE workspace_id = $1 AND action_id IN ($2, $3)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id_1)
    .bind(action_id_2)
    .fetch_one(&f.pool)
    .await
    .expect("episode count");
    assert_eq!(episode_count, 2, "exactly two episodes — one per action");
}

// ── T28: action-to-assignment 1:1 invariant (migration 0201) ──

/// T28a: First assignment for an action succeeds.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t28a_first_assignment_for_action_succeeds() {
    let f = setup().await.expect("fixture");
    let action_id = uuid::Uuid::now_v7();
    let experiment_uuid = uuid::Uuid::now_v7();
    insert_bare_assignment(
        &f.pool,
        f.workspace_id,
        action_id,
        experiment_uuid,
        "r/t28a",
    )
    .await;

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_experiment_assignments \
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("count");
    assert_eq!(count, 1, "first assignment for action should succeed");
}

/// T28b: Second assignment for the same (workspace_id, action_id) fails
/// with a uniqueness violation.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t28b_second_assignment_for_same_action_fails() {
    let f = setup().await.expect("fixture");
    let action_id = uuid::Uuid::now_v7();
    let experiment_uuid_1 = uuid::Uuid::now_v7();
    insert_bare_assignment(
        &f.pool,
        f.workspace_id,
        action_id,
        experiment_uuid_1,
        "r/t28b-1",
    )
    .await;

    // Second assignment with a DIFFERENT experiment_uuid but the SAME
    // action_id must fail due to the partial unique index.
    let experiment_uuid_2 = uuid::Uuid::now_v7();
    let result = sqlx::query(
        r#"INSERT INTO viryaos_experiment_assignments
           (id, workspace_id, experiment_uuid, unit_id, unit_kind,
            arm, intended_template_id, propensity, prediction, context, strategy,
            eligibility_criteria, selection_context, interference_policy,
            contamination_estimate, is_interference_controllable,
            experiment_status, execution_status, action_id)
           VALUES ($1,$2,$3,$4,'target_community','treatment','reddit-scanner',0.5,
                   '{}'::jsonb,'{}'::jsonb,'discovery',
                   '{}'::jsonb,'{}'::jsonb,'none',0.0,false,
                   'active','dispatched',$5)"#,
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(f.workspace_id.into_uuid())
    .bind(experiment_uuid_2)
    .bind("r/t28b-2")
    .bind(action_id)
    .execute(&f.pool)
    .await;

    assert!(
        result.is_err(),
        "second assignment for same (workspace_id, action_id) must fail"
    );
    let err = result.unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("unique")
            || err.to_string().to_lowercase().contains("duplicate"),
        "error should be a uniqueness violation, got: {err}"
    );
}

/// T28c: An action id belongs to exactly one workspace — the primary key
/// on `viryaos_autopilot_actions.id` makes cross-workspace reuse impossible.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t28c_action_id_cannot_repeat_across_workspaces() {
    let f = setup().await.expect("fixture");
    let action_id = uuid::Uuid::now_v7();

    insert_bare_assignment(
        &f.pool,
        f.workspace_id,
        action_id,
        uuid::Uuid::now_v7(),
        "r/t28c-ws1",
    )
    .await;

    let workspace_id_2 = WorkspaceId::new();
    let suffix = workspace_id_2.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id_2.into_uuid())
        .bind(format!("exp-integrity-ws2-{suffix}"))
        .bind("Second Workspace")
        .execute(&f.pool)
        .await
        .expect("insert workspace 2");

    // Reusing the id in a second workspace must be refused by
    // `viryaos_autopilot_actions_pkey`, which is on `id` alone.
    let decision_id_2 = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation)
           VALUES ($1,$2,$3,'growth_metrics','target_community',$4,
                   'auto_execute',9000,'auto_execute','test',
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb)"#,
    )
    .bind(decision_id_2)
    .bind(workspace_id_2.into_uuid())
    .bind(format!("decision-ws2-{action_id}"))
    .bind(uuid::Uuid::now_v7())
    .execute(&f.pool)
    .await
    .expect("insert decision in workspace 2");

    let repeated = sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status)
           VALUES ($1,$2,$3,'growth_metrics','signal.push.request','target_community',
                   $4,$5,'{}'::jsonb,'queued')"#,
    )
    .bind(action_id)
    .bind(workspace_id_2.into_uuid())
    .bind(decision_id_2)
    .bind(uuid::Uuid::now_v7())
    .bind(format!("action-ws2-{action_id}"))
    .execute(&f.pool)
    .await;
    let error = repeated.expect_err("a repeated action id must be refused");
    assert!(
        matches!(&error, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505")),
        "expected a unique violation, got {error:?}"
    );

    // So exactly one assignment can ever carry this action_id, and
    // `idx_experiment_assignments_action_id_unique` being keyed on
    // (workspace_id, action_id) is defence in depth rather than a case that
    // arises. The original test asserted the opposite — two workspaces sharing
    // one action_id — which the primary key has never permitted, so it could
    // not construct its own premise.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_experiment_assignments WHERE action_id = $1",
    )
    .bind(action_id)
    .fetch_one(&f.pool)
    .await
    .expect("count");
    assert_eq!(count, 1, "an action id belongs to exactly one workspace");
}

/// T28d: NULL action_id remains allowed (withheld / non-dispatched).
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t28d_null_action_id_remains_allowed() {
    let f = setup().await.expect("fixture");
    let experiment_uuid = uuid::Uuid::now_v7();
    // `fk_assignment_experiment` needs the design to exist first. The test
    // asserts the *partial* unique index tolerates repeated NULL action_ids,
    // and was failing on the foreign key before ever reaching that question.
    insert_experiment_design(&f.pool, f.workspace_id, experiment_uuid).await;

    // Insert two assignments with NULL action_id — both should succeed.
    for unit in ["r/t28d-1", "r/t28d-2"] {
        sqlx::query(
            r#"INSERT INTO viryaos_experiment_assignments
               (id, workspace_id, experiment_uuid, unit_id, unit_kind,
                arm, intended_template_id, propensity, prediction, context, strategy,
                eligibility_criteria, selection_context, interference_policy,
                contamination_estimate, is_interference_controllable,
                experiment_status, execution_status, action_id)
               VALUES ($1,$2,$3,$4,'target_community','control','reddit-scanner',0.5,
                       '{}'::jsonb,'{}'::jsonb,'discovery',
                       '{}'::jsonb,'{}'::jsonb,'none',0.0,false,
                       'active','control',NULL)"#,
        )
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(f.workspace_id.into_uuid())
        .bind(experiment_uuid)
        .bind(unit)
        .execute(&f.pool)
        .await
        .expect("insert control assignment with NULL action_id");
    }

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_experiment_assignments \
         WHERE workspace_id = $1 AND action_id IS NULL",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("count");
    assert_eq!(
        count, 2,
        "multiple NULL action_id assignments must be allowed"
    );
}

/// T28e: Existing experiment uniqueness (workspace_id, experiment_uuid,
/// assignment_round, unit_id) remains intact.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn t28e_experiment_unit_uniqueness_remains_intact() {
    let f = setup().await.expect("fixture");
    let experiment_uuid = uuid::Uuid::now_v7();
    let action_id_1 = uuid::Uuid::now_v7();

    insert_bare_assignment(
        &f.pool,
        f.workspace_id,
        action_id_1,
        experiment_uuid,
        "r/t28e",
    )
    .await;

    // Same (workspace_id, experiment_uuid, assignment_round=1, unit_id)
    // with a DIFFERENT action_id must still fail.
    let action_id_2 = uuid::Uuid::now_v7();
    let result = sqlx::query(
        r#"INSERT INTO viryaos_experiment_assignments
           (id, workspace_id, experiment_uuid, unit_id, unit_kind,
            arm, intended_template_id, propensity, prediction, context, strategy,
            eligibility_criteria, selection_context, interference_policy,
            contamination_estimate, is_interference_controllable,
            experiment_status, execution_status, action_id)
           VALUES ($1,$2,$3,$4,'target_community','treatment','reddit-scanner',0.5,
                   '{}'::jsonb,'{}'::jsonb,'discovery',
                   '{}'::jsonb,'{}'::jsonb,'none',0.0,false,
                   'active','dispatched',$5)"#,
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(f.workspace_id.into_uuid())
    .bind(experiment_uuid)
    .bind("r/t28e")
    .bind(action_id_2)
    .execute(&f.pool)
    .await;

    assert!(
        result.is_err(),
        "duplicate (workspace_id, experiment_uuid, assignment_round, unit_id) must fail"
    );
}
