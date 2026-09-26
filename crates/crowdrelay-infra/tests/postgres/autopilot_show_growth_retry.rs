//! A failed show-growth lever is retried, end to end through a real cycle.
//!
//! What only real rows prove: an event whose tracked-link attempt failed
//! gets a fresh attempt under its own key, instead of the failed row holding
//! the key and the ladder for good. On 2026-08-23 one failure at the Gorzów
//! show's tracked link stopped every lever for that show for a month.

use std::time::Duration;

use crate::common;
use crowdrelay_application::autopilot::EvaluateAutopilot;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use time::OffsetDateTime;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_failed_tracked_link_is_attempted_again() -> Result<(), Box<dyn std::error::Error>> {
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
    let ws = WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Retry')")
        .bind(ws.into_uuid())
        .bind(format!("retry-{}", ws.into_uuid().simple()))
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO growth_envelope (workspace_id, agent_enabled, dry_run) VALUES ($1, true, false)
         ON CONFLICT (workspace_id) DO UPDATE SET agent_enabled = true, dry_run = false",
    )
    .bind(ws.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO autopilot_policies
         (workspace_id, context, enabled, autonomy_level, max_actions_24h)
         VALUES ($1, 'show_growth', true, 'bounded_auto', 14)
         ON CONFLICT (workspace_id, context) DO UPDATE
         SET enabled = true, autonomy_level = 'bounded_auto', max_actions_24h = 14",
    )
    .bind(ws.into_uuid())
    .execute(&pool)
    .await?;
    let now = OffsetDateTime::now_utc();
    let event = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1, $2, $3, 'Show', $4, 'published', now())",
    )
    .bind(event)
    .bind(ws.into_uuid())
    .bind(format!("show-{}", event.simple()))
    .bind(now + time::Duration::days(17))
    .execute(&pool)
    .await?;

    // The August attempt: decided, tried, failed.
    let decision = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_decisions (
             id, workspace_id, decision_key, context, subject_kind, subject_id,
             decision_kind, confidence_basis_points, disposition, reason,
             input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id
         ) VALUES ($1,$2,$3,'show_growth','event',$4,'activate_show_growth_lever',9800,
                   'auto_execute','seeded','{}','{}','{}',now(),gen_random_uuid())",
    )
    .bind(decision)
    .bind(ws.into_uuid())
    .bind(format!("decision:seed-retry:{decision}"))
    .bind(event)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO autopilot_actions (
             id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
             idempotency_key, payload, status, action_class, approved_at, approved_by,
             available_at, finished_at, last_error_kind
         ) VALUES ($1,$2,$3,'show_growth','show.growth.request','event',$4,$5,$6,'failed',
                   'first_party_reversible',now(),'policy:bounded_auto',now(),now(),'unexpected')",
    )
    .bind(Uuid::now_v7())
    .bind(ws.into_uuid())
    .bind(decision)
    .bind(event)
    .bind(format!("action:show-growth:{event}:canonical_link_setup"))
    .bind(serde_json::json!({
        "kind": "request_show_growth",
        "event_id": event,
        "lever": "canonical_link_setup",
        "template_key": "show.growth.canonical_link.v1"
    }))
    .execute(&pool)
    .await?;

    EvaluateAutopilot::new(&repository, ws).execute(now).await?;

    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT idempotency_key FROM autopilot_actions
         WHERE workspace_id = $1 AND context = 'show_growth' AND status <> 'failed'",
    )
    .bind(ws.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        keys,
        [format!(
            "action:show-growth:{event}:canonical_link_setup:retry1"
        )],
        "the failed lever gets one more attempt under its own key"
    );
    Ok(())
}
