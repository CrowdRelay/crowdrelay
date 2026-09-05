//! An approval nobody can answer must not sit in the operator's queue.
//!
//! Production carried one: "Zatwierdź cel outreach: Unnamed target", 0%
//! confidence, whose own recorded reason is that all three Reddit searches
//! returned credential errors and no subreddit data was retrieved. The brain
//! had turned a connector failure into a proposal and asked a human to approve
//! it.
//!
//! `evaluate_outcome_quality` refuses to create these now — a `require_approval`
//! outcome with zero confidence produces no decision at all — but that guard is
//! prospective and left the rows already queued exactly where they were. Left
//! alone the row would have waited days for `approval_expires_at` to reap it,
//! and an exception queue that contains things the operator is supposed to
//! ignore stops being read at all.
//!
//! So the same rule runs against state, on the claim path, next to the sweep
//! that expires approvals whose deadline has passed.

use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;
use uuid::Uuid;

/// `(status, last_error_kind)`.
async fn action_state(
    pool: &sqlx::PgPool,
    action_id: Uuid,
) -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
    Ok(sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT status, last_error_kind FROM viryaos_autopilot_actions WHERE id = $1",
    )
    .bind(action_id)
    .fetch_one(pool)
    .await?)
}

/// An action awaiting approval, whose decision carries `confidence_basis_points`.
async fn seed_awaiting_approval(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    action_id: Uuid,
    confidence_basis_points: i32,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    let subject_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
        ) VALUES ($1,$2,$3,'booking_opportunity','agent_outcome',$4,
                  'outreach.target.request',$5,'require_approval',
                  'all three Reddit searches returned credential errors',
                  '{}','{}','{}',$6,$1)
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("decision-{decision_id}"))
    .bind(subject_id)
    .bind(confidence_basis_points)
    .bind(now)
    .execute(pool)
    .await?;

    // `approval_expires_at` is deliberately far in the future: the point is that
    // the row is cleared for being unanswerable, not for being old.
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
            idempotency_key, payload, status, available_at, approval_expires_at
        ) VALUES ($1,$2,$3,'booking_opportunity','outreach.target.request','agent_outcome',$4,
                  $5,$6,'awaiting_approval',$7,$7 + INTERVAL '30 days')
        "#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(subject_id)
    .bind(format!("action-{action_id}"))
    .bind(json!({"kind": "outreach.target.request"}))
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_zero_confidence_approval_is_cleared_and_a_supported_one_is_not()
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
            "unsupported-approval-{}",
            workspace_id.into_uuid().simple()
        ))
        .bind("Unsupported approval E2E")
        .execute(&pool)
        .await?;

    // The production shape: a connector failure dressed as a proposal.
    let unsupported = Uuid::now_v7();
    seed_awaiting_approval(&pool, workspace_id, unsupported, 0, now).await?;
    // A real proposal waiting on a human. It must survive: the sweep clears
    // approvals that cannot be answered, not the queue.
    let supported = Uuid::now_v7();
    seed_awaiting_approval(&pool, workspace_id, supported, 7_000, now).await?;

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);
    repository
        .claim_due_autonomous_actions(workspace_id, 32, now)
        .await?;

    let (status, error_kind) = action_state(&pool, unsupported).await?;
    assert_eq!(
        (status.as_str(), error_kind.as_deref()),
        ("cancelled", Some("insufficient_evidence")),
        "a zero-confidence approval must be cleared, and say why"
    );

    let (status, error_kind) = action_state(&pool, supported).await?;
    assert_eq!(
        (status.as_str(), error_kind.as_deref()),
        ("awaiting_approval", None),
        "an approval with evidence behind it must stay in the queue"
    );

    // No teardown: the workspace id is fresh per run and `viryaos_action_ledger`
    // holds a RESTRICT foreign key to it, so deleting would fail on rows the
    // claim path legitimately wrote.
    Ok(())
}
