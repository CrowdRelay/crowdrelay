//! The per-context daily quota is counted per action class, against a real
//! Postgres.
//!
//! What only real rows can prove: a context whose quota is full of
//! first-party work still lets an owned-audience action through; the quota
//! still throttles once the owned-audience class itself is full; and a row
//! with no recorded class counts against every class, so an unknown row is
//! never read as headroom.

use std::time::Duration;

use crate::common;
use crowdrelay_application::autopilot::{
    ActionSubject, AutopilotActionPayload, AutopilotContext, AutopilotDecisionRepository,
    DecisionCandidate,
};
use crowdrelay_domain::{
    EventId, TraceContext, WorkspaceId,
    autonomy::{Confidence, PolicyDisposition},
};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::PgPool;
use uuid::Uuid;

async fn workspace_with_quota(
    pool: &PgPool,
    quota: i32,
) -> Result<WorkspaceId, Box<dyn std::error::Error>> {
    let id = WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Quota')")
        .bind(id.into_uuid())
        .bind(format!("quota-{}", id.into_uuid().simple()))
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO autopilot_policies
         (workspace_id, context, enabled, autonomy_level, max_actions_24h)
         VALUES ($1, 'content_supply', true, 'bounded_auto', $2)
         ON CONFLICT (workspace_id, context) DO UPDATE
         SET enabled = true, autonomy_level = 'bounded_auto', max_actions_24h = $2",
    )
    .bind(id.into_uuid())
    .bind(quota)
    .execute(pool)
    .await?;
    Ok(id)
}

/// One action of the context already taken today, with the class it was
/// written under — `None` for a row that recorded none.
async fn seed_action(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    class: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
        ) VALUES ($1,$2,$3,'content_supply','workspace',$2,
                  'request_content_artifact',9000,'auto_execute','seeded',
                  '{}','{}','{}',now(),gen_random_uuid())
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("decision:seed-quota:{decision_id}"))
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
            idempotency_key, payload, status, action_class, approved_at, approved_by,
            available_at, finished_at
        ) VALUES ($1,$2,$3,'content_supply','content.artifact.request','workspace',$2,$4,
                  '{}','succeeded',$5,now(),'policy:bounded_auto',now(),now())
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(format!("action:seed-quota:{decision_id}"))
    .bind(class)
    .execute(pool)
    .await?;
    Ok(())
}

/// A relay of the band's own post as a push to fans who opted in — the
/// owned-audience work the render backlog starved. A fresh subject each
/// time, so the in-flight index never collides two pushes.
fn relay_push() -> DecisionCandidate {
    let nonce = Uuid::now_v7();
    DecisionCandidate {
        context: AutopilotContext::ContentSupply,
        subject: ActionSubject::Event(EventId::from_uuid(nonce)),
        decision_kind: "relay_owned_post",
        confidence: Confidence::from_basis_points(9_000).expect("valid basis points"),
        disposition: PolicyDisposition::AutoExecute,
        reason: "band published a post on an owned account — carried to fans who opted in",
        input_snapshot: serde_json::json!({}),
        policy_snapshot: serde_json::json!({}),
        action: AutopilotActionPayload::RequestSignalPush {
            task_id: nonce,
            title: "Virya".to_owned(),
            body: "New post".to_owned(),
            target_path: None,
            event_id: None,
            segment: None,
            audience_size: None,
            audience_basis: String::new(),
        },
        decision_key: format!("decision:test-quota:{nonce}"),
        action_idempotency_key: format!("action:test-quota:{nonce}"),
    }
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_render_backlog_does_not_spend_the_relay_quota() -> Result<(), Box<dyn std::error::Error>>
{
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let repository = PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: database_url,
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(5),
            lock_timeout: Duration::from_secs(1),
        },
    );
    let persist = |ws: WorkspaceId, candidate: DecisionCandidate| {
        let repository = &repository;
        async move {
            repository
                .persist_candidate(ws, &candidate, &TraceContext::root(ws))
                .await
        }
    };

    // The day's quota of two is full of renders; the push still goes.
    let ws = workspace_with_quota(&pool, 2).await?;
    seed_action(&pool, ws, Some("first_party_reversible")).await?;
    seed_action(&pool, ws, Some("first_party_reversible")).await?;
    let first = persist(ws, relay_push()).await?;
    assert!(
        first.action_created,
        "renders do not spend the relay's quota"
    );
    assert!(!first.quota_throttled);
    assert!(persist(ws, relay_push()).await?.action_created);

    // The owned-audience class is now full on its own: the quota holds.
    let third = persist(ws, relay_push()).await?;
    assert!(!third.action_created);
    assert!(third.quota_throttled);

    // Rows with no recorded class count against every class.
    let unknown = workspace_with_quota(&pool, 2).await?;
    seed_action(&pool, unknown, None).await?;
    seed_action(&pool, unknown, None).await?;
    let held = persist(unknown, relay_push()).await?;
    assert!(!held.action_created);
    assert!(held.quota_throttled, "an unknown row is never headroom");
    Ok(())
}
