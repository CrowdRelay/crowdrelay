use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;
use uuid::Uuid;

/// A capability nobody advertises is an operator's decision, not a fault, and
/// claiming an action anyway spends one of the five attempts it is allowed.
/// Five cycles later the action is `failed` for good: the content snapshot stops
/// counting it as in flight, and the decision that replaces it dedupes into an
/// idempotency key that is already taken. Enabling the gate afterwards would not
/// bring the work back.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn gated_actions_are_parked_without_spending_an_attempt()
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
            "gated-claim-e2e-{}",
            workspace_id.into_uuid().simple()
        ))
        .bind("Gated claim E2E")
        .execute(&pool)
        .await?;

    // A registry that advertises one capability and not the other, which is
    // exactly the production shape: n8n is up and fails closed on the gates the
    // operator has not switched on.
    sqlx::query(
        r#"
        INSERT INTO viryaos_executor_instances (
            workspace_id, executor_id, version, manifest_sha, observed_at, expires_at
        ) VALUES ($1,'n8n-gated-test','test','test-manifest',$2,$3)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(now + time::Duration::minutes(30))
    .execute(&pool)
    .await?;
    advertise(&pool, workspace_id, "fan.lifecycle.message", now).await?;

    let gated_action_id = Uuid::now_v7();
    let advertised_action_id = Uuid::now_v7();
    let executor_free_action_id = Uuid::now_v7();
    seed_action(
        &pool,
        workspace_id,
        gated_action_id,
        "content.artifact.request",
        json!({
            "kind": "request_content_artifact",
            "source_id": Uuid::now_v7(),
            "source_version": 1,
            "artifact": "live_listing",
            "template_key": "content.live_listing.v1"
        }),
        now,
    )
    .await?;
    seed_action(
        &pool,
        workspace_id,
        advertised_action_id,
        "fan.lifecycle.message.request",
        json!({
            "kind": "request_fan_lifecycle_message",
            "fan_id": Uuid::now_v7(),
            "template_key": "test"
        }),
        now,
    )
    .await?;
    seed_action(
        &pool,
        workspace_id,
        executor_free_action_id,
        "growth.debt.raise",
        json!({
            "kind": "raise_growth_debt",
            "subject_kind": "relationship",
            "subject_id": Uuid::now_v7(),
            "debt_kind": "relationship_quiet",
            "recommended_action": "reach out",
            "overdue_basis_points": 15000,
            "outstanding_items": 3,
            "tracked_items": 9,
            "priority": 40,
            "template_key": "growth.debt.v1"
        }),
        now,
    )
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
        .claim_due_autonomous_actions(workspace_id, 32, now)
        .await?;
    let claimed_ids: Vec<Uuid> = claimed.iter().map(|action| action.id.into_uuid()).collect();
    assert!(claimed_ids.contains(&advertised_action_id));
    assert!(claimed_ids.contains(&executor_free_action_id));
    assert!(!claimed_ids.contains(&gated_action_id));

    let (status, attempts, error_kind, waits) = action_state(&pool, gated_action_id).await?;
    assert_eq!(status, "queued");
    assert_eq!(attempts, 0);
    assert_eq!(error_kind.as_deref(), Some("awaiting_executor"));
    assert!(
        waits,
        "a parked action must not be reconsidered immediately"
    );

    // Polling again inside the park window neither claims it nor burns an
    // attempt, which is what turned a gated capability into a dead action.
    let repeat = repository
        .claim_due_autonomous_actions(workspace_id, 32, now + time::Duration::seconds(60))
        .await?;
    assert!(
        !repeat
            .iter()
            .any(|action| action.id.into_uuid() == gated_action_id)
    );
    let (_, attempts_after_repeat, _, _) = action_state(&pool, gated_action_id).await?;
    assert_eq!(attempts_after_repeat, 0);

    // The moment the operator switches the capability on, the parked work is
    // claimed with its full retry budget intact.
    advertise(&pool, workspace_id, "content.artifact", now).await?;
    let after_gate_opens = repository
        .claim_due_autonomous_actions(workspace_id, 32, now + time::Duration::minutes(6))
        .await?;
    assert!(
        after_gate_opens
            .iter()
            .any(|action| action.id.into_uuid() == gated_action_id)
    );
    let (status, attempts, _, _) = action_state(&pool, gated_action_id).await?;
    assert_eq!(status, "processing");
    assert_eq!(attempts, 1);

    // Clean up: the action ledger has ON DELETE RESTRICT on workspace_id,
    // so we must delete the actions (which cascade to the ledger) before
    // the workspace can be removed.
    sqlx::query("DELETE FROM viryaos_autopilot_actions WHERE workspace_id = $1")
        .bind(workspace_id.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM viryaos_autopilot_decisions WHERE workspace_id = $1")
        .bind(workspace_id.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(workspace_id.into_uuid())
        .execute(&pool)
        .await?;
    Ok(())
}

async fn advertise(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    capability: &str,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        r#"
        INSERT INTO viryaos_executor_capabilities (
            workspace_id, executor_id, capability, capability_version, observed_at, expires_at
        ) VALUES ($1,'n8n-gated-test',$2,'1',$3,$4)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(capability)
    .bind(now)
    .bind(now + time::Duration::minutes(30))
    .execute(pool)
    .await?;
    Ok(())
}

async fn action_state(
    pool: &sqlx::PgPool,
    action_id: Uuid,
) -> Result<(String, i32, Option<String>, bool), Box<dyn std::error::Error>> {
    let row = sqlx::query_as::<_, (String, i32, Option<String>, bool)>(
        r#"
        SELECT status, attempt_count, last_error_kind, available_at > now()
        FROM viryaos_autopilot_actions
        WHERE id = $1
        "#,
    )
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

async fn seed_action(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    action_id: Uuid,
    action_kind: &str,
    payload: Value,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    let subject_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at
        ) VALUES ($1,$2,$3,'content_supply','test_subject',$4,$5,10000,
                  'auto_execute','gated claim regression','{}','{}','{}',$6)
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
        ) VALUES ($1,$2,$3,'content_supply',$4,'test_subject',$5,$6,$7,
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
