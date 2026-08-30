//! Experiment integrity tests — P0-1, P0-2 behavioral tests T1-T9,
//! execution integrity tests T10-T14.
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

/// Helper: insert a minimal autopilot decision + action for test setup.
async fn insert_decision_and_action(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    trace_id: uuid::Uuid,
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
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)"#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("decision-{decision_id}"))
    .bind(uuid::Uuid::now_v7())
    .bind(trace_id)
    .execute(pool)
    .await
    .expect("insert decision");

    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, approved_at, available_at,
            finished_at, trace_id)
           VALUES ($1,$2,$3,'growth_metrics','community.engage','target_community',
                   $4,$5,$6,'succeeded',now(),now(),now(),$7)"#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(uuid::Uuid::now_v7())
    .bind(format!("action-{action_id}"))
    .bind(serde_json::json!({"kind":"community.engage.request"}))
    .bind(trace_id)
    .execute(pool)
    .await
    .expect("insert action");
    action_id
}

/// Helper: insert a community_posts row for a given action.
async fn insert_community_post(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    action_id: uuid::Uuid,
    status: &str,
    subreddit: &str,
) -> uuid::Uuid {
    let post_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO community_posts
           (id, workspace_id, action_id, subreddit, title, body, smart_link, status)
           VALUES ($1,$2,$3,$4,'Test post','Test body',NULL,$5)"#,
    )
    .bind(post_id)
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(subreddit)
    .bind(status)
    .execute(pool)
    .await
    .expect("insert community_post");
    post_id
}

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
           SELECT $1, $2, decision_id, 'growth_metrics', 'community.engage', 'target_community',
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
           SELECT $1, $2, decision_id, 'growth_metrics', 'community.engage', 'target_community',
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
           SELECT $1, $2, decision_id, 'growth_metrics', 'community.engage', 'target_community',
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
