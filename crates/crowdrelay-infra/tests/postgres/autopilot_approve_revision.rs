use std::collections::BTreeMap;
use std::time::Duration;

use crate::common;
use crowdrelay_application::IdempotencyKey;
use crowdrelay_application::autopilot::{AutopilotActionRepository, AutopilotControlRepository};
use crowdrelay_domain::{AutopilotActionId, WorkspaceId};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use time::OffsetDateTime;
use uuid::Uuid;

async fn repository()
-> Result<(PostgresAutopilotRepository, sqlx::PgPool), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let database = DatabaseConfig {
        url: database_url.clone(),
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
    };
    Ok((
        PostgresAutopilotRepository::new(pool.clone(), &database),
        pool,
    ))
}

async fn seed_workspace(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("revise-{}", workspace_id.into_uuid().simple()))
        .bind("Revision Test")
        .execute(pool)
        .await?;
    Ok(())
}

/// A parked action whose payload carries revisable words under `draft`.
async fn seed_awaiting_action(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    draft_text: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision_id: Uuid = sqlx::query_scalar(
        "INSERT INTO autopilot_decisions
             (id, workspace_id, decision_key, context, subject_kind, subject_id,
              decision_kind, confidence_basis_points, disposition, reason,
              input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
         VALUES ($1,$2,$3,'content_supply','content_source',$4,
                 'seed.revision',9000,'require_approval','seeded revision target',
                 '{}','{}','{}',now(),$5) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("revise-decision-{}", Uuid::now_v7()))
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .fetch_one(pool)
    .await?;
    sqlx::query_scalar(
        "INSERT INTO autopilot_actions
             (id, workspace_id, decision_id, context, action_kind, subject_kind,
              subject_id, idempotency_key, payload, status)
         VALUES ($1,$2,$3,'content_supply','agent.content.draft','content_source',
                 $4,$5,$6,'awaiting_approval') RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .bind(format!("revise-action-{}", Uuid::now_v7()))
    .bind(serde_json::json!({
        "kind": "request_agent_content",
        "task_id": Uuid::now_v7(),
        "draft": {
            "draft_text": draft_text,
            "subject": "Wiadomość od zespołu",
        },
        "recipient_email": "journalist@example.com",
    }))
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn approve_with_revision_rewrites_payload_and_records_distance()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    let action_id = seed_awaiting_action(&pool, workspace_id, "Check out our new single").await?;

    let mut revision = BTreeMap::new();
    revision.insert(
        "draft_text".to_owned(),
        "Posłuchaj naszego nowego singla".to_owned(),
    );

    let mutation = repo
        .approve_action(
            workspace_id,
            AutopilotActionId::from_uuid(action_id),
            &IdempotencyKey::parse(format!("revise-{}", action_id.simple()))?,
            None,
            Some(&revision),
        )
        .await?;
    assert_eq!(mutation.status, "queued");
    assert!(!mutation.replayed);

    let (status, payload): (String, serde_json::Value) =
        sqlx::query_as("SELECT status, payload FROM autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(status, "queued");
    assert_eq!(
        payload["draft"]["draft_text"],
        serde_json::json!("Posłuchaj naszego nowego singla")
    );
    // Fields the operator did not touch stay as the machine wrote them —
    // a revision rewrites words, never the send's routing.
    assert_eq!(
        payload["recipient_email"],
        serde_json::json!("journalist@example.com")
    );

    let rows: Vec<(String, String, String, i32)> = sqlx::query_as(
        "SELECT field, before_text, after_text, distance_chars
         FROM draft_revisions WHERE action_id = $1",
    )
    .bind(action_id)
    .fetch_all(&pool)
    .await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "draft_text");
    assert_eq!(rows[0].1, "Check out our new single");
    assert_eq!(rows[0].2, "Posłuchaj naszego nowego singla");
    assert!(rows[0].3 > 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn approve_with_revision_refuses_locked_field_and_keeps_draft()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    let action_id = seed_awaiting_action(&pool, workspace_id, "Check out our new single").await?;

    let mut revision = BTreeMap::new();
    revision.insert("recipient_email".to_owned(), "other@example.com".to_owned());

    let refused = repo
        .approve_action(
            workspace_id,
            AutopilotActionId::from_uuid(action_id),
            &IdempotencyKey::parse(format!("revise-{}", action_id.simple()))?,
            None,
            Some(&revision),
        )
        .await;
    assert!(matches!(
        refused,
        Err(crowdrelay_application::RepositoryError::ConflictBecause(_))
    ));

    let (status, payload): (String, serde_json::Value) =
        sqlx::query_as("SELECT status, payload FROM autopilot_actions WHERE id = $1")
            .bind(action_id)
            .fetch_one(&pool)
            .await?;
    // The refusal refuses the approval, not just the edit — approving the
    // original words the operator tried to change would approve something
    // nobody approved.
    assert_eq!(status, "awaiting_approval");
    assert_eq!(
        payload["recipient_email"],
        serde_json::json!("journalist@example.com")
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn approve_with_identical_revision_is_refused() -> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    let action_id = seed_awaiting_action(&pool, workspace_id, "Check out our new single").await?;

    let mut revision = BTreeMap::new();
    revision.insert(
        "draft_text".to_owned(),
        "Check out our new single".to_owned(),
    );

    let refused = repo
        .approve_action(
            workspace_id,
            AutopilotActionId::from_uuid(action_id),
            &IdempotencyKey::parse(format!("revise-{}", action_id.simple()))?,
            None,
            Some(&revision),
        )
        .await;
    assert!(matches!(
        refused,
        Err(crowdrelay_application::RepositoryError::ConflictBecause(_))
    ));
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn approve_with_revision_replays_under_the_same_key() -> Result<(), Box<dyn std::error::Error>>
{
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    let action_id = seed_awaiting_action(&pool, workspace_id, "Check out our new single").await?;

    let key = IdempotencyKey::parse(format!("revise-{}", action_id.simple()))?;
    let mut revision = BTreeMap::new();
    revision.insert(
        "draft_text".to_owned(),
        "Posłuchaj naszego nowego singla".to_owned(),
    );

    repo.approve_action(
        workspace_id,
        AutopilotActionId::from_uuid(action_id),
        &key,
        None,
        Some(&revision),
    )
    .await?;
    let replayed = repo
        .approve_action(
            workspace_id,
            AutopilotActionId::from_uuid(action_id),
            &key,
            None,
            Some(&revision),
        )
        .await?;
    assert!(replayed.replayed);

    let revision_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM draft_revisions WHERE action_id = $1")
            .bind(action_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(revision_rows, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn pending_actions_expose_the_revisable_surface() -> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    let action_id = seed_awaiting_action(&pool, workspace_id, "Check out our new single").await?;

    let pipeline = repo.load_content_pipeline(workspace_id).await?;
    let pending = pipeline
        .pending
        .iter()
        .find(|action| action.id.into_uuid() == action_id)
        .expect("seeded action is pending");
    assert_eq!(
        pending.revisable.get("draft_text").map(String::as_str),
        Some("Check out our new single")
    );
    assert_eq!(
        pending.revisable.get("subject").map(String::as_str),
        Some("Wiadomość od zespołu")
    );
    // The recipient is not on the surface — the modal and the gate share one
    // allowlist, so a field that cannot be revised is never rendered editable.
    assert!(!pending.revisable.contains_key("recipient_email"));
    Ok(())
}

/// A queued action whose stored class and payload variant disagree on
/// purpose — the gate reads the durable class, never the payload's claims.
async fn seed_classed_action(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    action_class: &str,
    payload: serde_json::Value,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision_id: Uuid = sqlx::query_scalar(
        "INSERT INTO autopilot_decisions
             (id, workspace_id, decision_key, context, subject_kind, subject_id,
              decision_kind, confidence_basis_points, disposition, reason,
              input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
         VALUES ($1,$2,$3,'content_supply','content_source',$4,
                 'seed.gate',9000,'require_approval','seeded gate target',
                 '{}','{}','{}',now(),$5) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("gate-decision-{}", Uuid::now_v7()))
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .fetch_one(pool)
    .await?;
    sqlx::query_scalar(
        "INSERT INTO autopilot_actions
             (id, workspace_id, decision_id, context, action_kind, subject_kind,
              subject_id, idempotency_key, payload, status, action_class)
         VALUES ($1,$2,$3,'content_supply','agent.content.draft','content_source',
                 $4,$5,$6,'queued',$7) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .bind(format!("gate-action-{}", Uuid::now_v7()))
    .bind(payload)
    .bind(action_class)
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

/// 2.1 — an action recorded as third-party whose dispatch would carry no
/// evidence cannot leave the building. The payload here is an internal
/// channel draft — no recipient — so its arm attaches nothing; the stored
/// class is what the gate enforces, not what the payload happens to be.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn outward_send_without_evidence_is_refused_at_dispatch()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    let action_id = seed_classed_action(
        &pool,
        workspace_id,
        "third_party",
        serde_json::json!({
            "kind": "request_agent_content",
            "task_id": Uuid::now_v7(),
            "draft": {"draft_text": "channel draft, no recipient"},
        }),
    )
    .await?;

    let claimed = repo
        .claim_due_autonomous_actions(workspace_id, 8, OffsetDateTime::now_utc())
        .await?;
    let action = claimed
        .into_iter()
        .find(|claimed| claimed.id.into_uuid() == action_id)
        .expect("queued action is claimable");
    let outcome = repo
        .execute_action(workspace_id, &action, OffsetDateTime::now_utc())
        .await;
    assert!(
        matches!(
            outcome,
            Err(crowdrelay_application::RepositoryError::ConflictBecause(_))
        ),
        "expected the evidence gate to refuse, got {outcome:?}"
    );

    let outbox_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_events
         WHERE workspace_id = $1 AND payload->>'action_id' = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.to_string())
    .fetch_one(&pool)
    .await?;
    assert_eq!(outbox_rows, 0, "the refused send left no outbox intent");
    Ok(())
}

/// 2.2 — the same words to a second journalist is a broadcast wearing a
/// pitch's costume. The first send stands; the second is refused at
/// dispatch with the draft compared byte-for-byte.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn identical_third_party_draft_is_refused_at_dispatch()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    let draft = serde_json::json!({
        "draft_text": "We would love coverage of the new single",
        "subject": "New single",
    });
    let make_payload = |email: &str| {
        serde_json::json!({
            "kind": "request_agent_content",
            "task_id": Uuid::now_v7(),
            "draft": draft,
            "recipient_email": email,
        })
    };
    let first = seed_classed_action(
        &pool,
        workspace_id,
        "third_party",
        make_payload("a@example.com"),
    )
    .await?;
    let second = seed_classed_action(
        &pool,
        workspace_id,
        "third_party",
        make_payload("b@example.com"),
    )
    .await?;

    let claimed = repo
        .claim_due_autonomous_actions(workspace_id, 8, OffsetDateTime::now_utc())
        .await?;
    let first_action = claimed
        .iter()
        .find(|claimed| claimed.id.into_uuid() == first)
        .expect("first queued action is claimable");
    repo.execute_action(workspace_id, first_action, OffsetDateTime::now_utc())
        .await?;

    // Both actions were claimed in one batch; the second executes on the same
    // claim — a re-claim would find it already leased.
    let second_action = claimed
        .iter()
        .find(|claimed| claimed.id.into_uuid() == second)
        .expect("second queued action is claimable");
    let outcome = repo
        .execute_action(workspace_id, second_action, OffsetDateTime::now_utc())
        .await;
    assert!(
        matches!(
            outcome,
            Err(crowdrelay_application::RepositoryError::ConflictBecause(_))
        ),
        "expected the identical-draft refusal, got {outcome:?}"
    );
    Ok(())
}
