use std::time::Duration;

use crowdrelay_application::{
    RepositoryError,
    autopilot::{
        AutopilotActionPayload, AutopilotActionRepository, AutopilotRuntimeRepository,
        ClaimExecution, ExecutorReportStatus, RecordExecutionReport,
    },
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn queued_team_assignment_email_uses_fast_lane_and_emits_bridge_event()
-> Result<(), Box<dyn std::error::Error>> {
    let database_url =
        std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").map_err(|error| {
            format!(
                "CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL must target a disposable database: {error}"
            )
        })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

    let workspace_id = WorkspaceId::new();
    let now = OffsetDateTime::now_utc();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!(
            "team-email-e2e-{}",
            workspace_id.into_uuid().simple()
        ))
        .bind("Team email dispatch E2E")
        .execute(&pool)
        .await?;

    sqlx::query(
        r#"
        INSERT INTO viryaos_executor_instances (
            workspace_id, executor_id, version, manifest_sha, observed_at, expires_at
        ) VALUES ($1,'n8n-team-email-test','test','test-manifest',$2,$3)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(now + time::Duration::minutes(10))
    .execute(&pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO viryaos_executor_capabilities (
            workspace_id, executor_id, capability, capability_version, observed_at, expires_at
        ) VALUES ($1,'n8n-team-email-test','team.email','1',$2,$3)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(now + time::Duration::minutes(10))
    .execute(&pool)
    .await?;

    let assignment_id = Uuid::now_v7();
    let team_action_id = Uuid::now_v7();
    let unrelated_action_id = Uuid::now_v7();
    seed_action(
        &pool,
        workspace_id,
        team_action_id,
        assignment_id,
        "team.assignment.email",
        serde_json::to_value(AutopilotActionPayload::SendTeamAssignmentEmail {
            assignment_id,
            recipient_email: "member1@example.test".to_owned(),
            recipient_name: "Member One".to_owned(),
            task_title: "Check booking request".to_owned(),
            task_detail: "Open Needs you and review the booking request.".to_owned(),
            due_at: None,
            action_url_path: "/staff/control/".to_owned(),
            reminder_number: 0,
        })?,
        now,
    )
    .await?;
    seed_action(
        &pool,
        workspace_id,
        unrelated_action_id,
        Uuid::now_v7(),
        "fan.lifecycle.message.request",
        json!({
            "kind": "request_fan_lifecycle_message",
            "fan_id": Uuid::now_v7(),
            "template_key": "test"
        }),
        now,
    )
    .await?;
    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status='processing', attempt_count=5, started_at=$3 WHERE workspace_id=$1 AND id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(unrelated_action_id)
    .bind(now - time::Duration::minutes(20))
    .execute(&pool)
    .await?;

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);

    let claimed = repository
        .claim_due_team_email_actions(workspace_id, 32, now)
        .await?;
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id.into_uuid(), team_action_id);
    assert_eq!(claimed[0].payload.action_kind(), "team.assignment.email");

    let unrelated_status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM viryaos_autopilot_actions WHERE workspace_id=$1 AND id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(unrelated_action_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(unrelated_status, "processing");

    repository
        .execute_action(workspace_id, &claimed[0], OffsetDateTime::now_utc())
        .await?;

    let action_status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM viryaos_autopilot_actions WHERE workspace_id=$1 AND id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(team_action_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(action_status, "succeeded");

    let outbox_payload = sqlx::query_scalar::<_, Value>(
        r#"
        SELECT payload
        FROM outbox_events
        WHERE workspace_id=$1 AND event_type='crowdrelay.team.assignment_email_requested'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(outbox_payload["action_id"], team_action_id.to_string());
    assert_eq!(outbox_payload["assignment_id"], assignment_id.to_string());
    assert_eq!(outbox_payload["recipient_email"], "member1@example.test");

    let emission_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM viryaos_autopilot_action_emissions WHERE workspace_id=$1 AND action_id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(team_action_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(emission_count, 1);

    // Execution ownership is closed-loop and provider success is monotonic.
    // This simulates duplicate delivery, a worker/provider ambiguity window and
    // a delayed failure receipt arriving after Gmail already confirmed success.
    let executor_id = "n8n-team-email-test".to_owned();
    let action_id = claimed[0].id;
    let first_claim = repository
        .claim_execution(
            workspace_id,
            ClaimExecution {
                action_id,
                executor_id: executor_id.clone(),
                occurred_at: now,
            },
        )
        .await?;
    assert_eq!(first_claim.disposition, "claimed");
    assert_eq!(first_claim.attempt_number, 1);
    let claim_token = first_claim.claim_token.ok_or("missing first claim token")?;

    let duplicate_claim = repository
        .claim_execution(
            workspace_id,
            ClaimExecution {
                action_id,
                executor_id: executor_id.clone(),
                occurred_at: now + time::Duration::minutes(1),
            },
        )
        .await?;
    assert_eq!(duplicate_claim.disposition, "in_flight");
    assert!(duplicate_claim.claim_token.is_none());

    let ambiguous_claim = repository
        .claim_execution(
            workspace_id,
            ClaimExecution {
                action_id,
                executor_id: executor_id.clone(),
                occurred_at: now + time::Duration::minutes(16),
            },
        )
        .await?;
    assert_eq!(ambiguous_claim.disposition, "ambiguous");
    assert!(ambiguous_claim.claim_token.is_none());

    let wrong_token = repository
        .record_execution_report(
            workspace_id,
            RecordExecutionReport {
                action_id,
                receipt_key: format!("wrong-token-{team_action_id}"),
                executor_id: executor_id.clone(),
                status: ExecutorReportStatus::Failed,
                claim_token: Some(Uuid::now_v7()),
                provider_reference: None,
                error_kind: Some("simulated_transport_failure".to_owned()),
                metadata: json!({"test": true}),
                occurred_at: now + time::Duration::minutes(2),
            },
        )
        .await;
    assert!(matches!(wrong_token, Err(RepositoryError::Conflict)));

    let success_receipt = format!("gmail-success-{team_action_id}");
    let success = repository
        .record_execution_report(
            workspace_id,
            RecordExecutionReport {
                action_id,
                receipt_key: success_receipt.clone(),
                executor_id: executor_id.clone(),
                status: ExecutorReportStatus::Succeeded,
                claim_token: Some(claim_token),
                provider_reference: Some("gmail-message-123".to_owned()),
                error_kind: None,
                metadata: json!({"provider": "gmail"}),
                occurred_at: now + time::Duration::minutes(2),
            },
        )
        .await?;
    assert!(!success.replayed);

    let replayed_success = repository
        .record_execution_report(
            workspace_id,
            RecordExecutionReport {
                action_id,
                receipt_key: success_receipt,
                executor_id: executor_id.clone(),
                status: ExecutorReportStatus::Succeeded,
                claim_token: Some(claim_token),
                provider_reference: Some("gmail-message-123".to_owned()),
                error_kind: None,
                metadata: json!({"provider": "gmail"}),
                occurred_at: now + time::Duration::minutes(2),
            },
        )
        .await?;
    assert!(replayed_success.replayed);

    let delayed_failure = repository
        .record_execution_report(
            workspace_id,
            RecordExecutionReport {
                action_id,
                receipt_key: format!("delayed-failure-{team_action_id}"),
                executor_id: executor_id.clone(),
                status: ExecutorReportStatus::Failed,
                claim_token: Some(claim_token),
                provider_reference: None,
                error_kind: Some("late_transport_timeout".to_owned()),
                metadata: json!({"test": "delayed-after-success"}),
                occurred_at: now + time::Duration::minutes(3),
            },
        )
        .await?;
    assert!(!delayed_failure.replayed);

    let after_success = repository
        .claim_execution(
            workspace_id,
            ClaimExecution {
                action_id,
                executor_id: executor_id.clone(),
                occurred_at: now + time::Duration::minutes(20),
            },
        )
        .await?;
    assert_eq!(after_success.disposition, "already_succeeded");
    assert_eq!(after_success.attempt_number, 1);
    assert_eq!(
        after_success.provider_reference.as_deref(),
        Some("gmail-message-123")
    );

    let claim_state = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT status, provider_reference FROM viryaos_autopilot_execution_claims \
         WHERE workspace_id=$1 AND action_id=$2 AND executor_id=$3",
    )
    .bind(workspace_id.into_uuid())
    .bind(team_action_id)
    .bind(&executor_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(claim_state.0, "succeeded");
    assert_eq!(claim_state.1.as_deref(), Some("gmail-message-123"));

    pool.close().await;
    Ok(())
}

async fn seed_action(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    action_id: Uuid,
    subject_id: Uuid,
    action_kind: &str,
    payload: Value,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at
        ) VALUES ($1,$2,$3,'booking_opportunity','test_subject',$4,$5,10000,
                  'auto_execute','team email dispatch regression','{}','{}','{}',$6)
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("decision-{decision_id}"))
    .bind(subject_id)
    .bind(format!("test.{action_kind}"))
    .bind(now)
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
            idempotency_key, payload, status, approved_at, approved_by, available_at
        ) VALUES ($1,$2,$3,'booking_opportunity',$4,'test_subject',$5,$6,$7,
                  'queued',$8,'system:test',$8)
        "#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(action_kind)
    .bind(subject_id)
    .bind(format!("action-{action_id}"))
    .bind(payload)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}
