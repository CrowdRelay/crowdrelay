//! Terminal outbox events that a durable record still names are not garbage.
//!
//! `autopilot_action_emissions`, `show_notification_emissions`,
//! `calendar_requests` and `communication_campaigns` all hold RESTRICT
//! foreign keys into `outbox_events`: the row may not vanish while the ledger
//! or campaign still points at it. The retention step never accounted for
//! them, so the first emitted action that aged past the terminal window turned
//! every retention cycle into `error_kind=database` — reproduced in production
//! on 2026-09-18. These tests run the real worker's `run_once`, because a test
//! that reimplemented the query would prove nothing about the one that ships.

mod common;

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use crowdrelay_worker::retention::{RetentionWorker, RetentionWorkerConfig};
use sqlx::PgPool;
use uuid::Uuid;

async fn workspace(pool: &PgPool) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("retention-{}", id.simple()))
        .bind("Retention")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(id)
}

async fn terminal_event(pool: &PgPool, workspace_id: Uuid, event_type: &str) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO outbox_events (
            id, workspace_id, event_type, event_version, payload, request_id,
            status, delivered_at, available_at
        ) VALUES ($1, $2, $3, 1, '{}'::jsonb, $4, 'delivered',
                  now() - interval '40 days', now() - interval '40 days')
        "#,
    )
    .bind(id)
    .bind(workspace_id)
    .bind(event_type)
    .bind(format!("request-{id}"))
    .execute(pool)
    .await
    .context("insert terminal outbox event")?;
    Ok(id)
}

async fn action_emission(pool: &PgPool, workspace_id: Uuid, event_id: Uuid) -> Result<()> {
    let decision_id = Uuid::now_v7();
    let action_id = Uuid::now_v7();
    let trace_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id
        ) VALUES ($1,$2,$3,'outreach','workspace',$4,
                  'contact.attempt',9000,'auto_execute','send the note',
                  '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id)
    .bind(format!("decision-{decision_id}"))
    .bind(workspace_id)
    .bind(trace_id)
    .execute(pool)
    .await
    .context("insert decision")?;
    sqlx::query(
        r#"
        INSERT INTO autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, finished_at, trace_id
        ) VALUES ($1,$2,$3,'outreach','contact.attempt','workspace',
                  $4,$5,$6,'succeeded',now(),$7)
        "#,
    )
    .bind(action_id)
    .bind(workspace_id)
    .bind(decision_id)
    .bind(workspace_id)
    .bind(format!("action-{action_id}"))
    .bind(serde_json::json!({"kind":"contact.attempt"}))
    .bind(trace_id)
    .execute(pool)
    .await
    .context("insert action")?;
    sqlx::query(
        r#"
        INSERT INTO autopilot_action_emissions (
            workspace_id, action_id, emission_key, outbox_event_id
        ) VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(workspace_id)
    .bind(action_id)
    .bind(format!("emission-{action_id}"))
    .bind(event_id)
    .execute(pool)
    .await
    .context("insert action emission")?;
    Ok(())
}

fn worker(pool: &PgPool) -> Result<RetentionWorker> {
    RetentionWorker::new(
        pool.clone(),
        RetentionWorkerConfig {
            poll_interval: Duration::from_secs(3600),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(5),
            terminal_outbox_retention: Duration::from_secs(30 * 24 * 3600),
            consumed_token_retention: Duration::from_secs(30 * 24 * 3600),
            terminal_push_retention: Duration::from_secs(30 * 24 * 3600),
            decision_audit_retention: Duration::from_secs(180 * 24 * 3600),
            batch_size: 500,
        },
    )
    .context("build retention worker")
}

/// The step that used to fail must now run: the event the emission ledger
/// still names is kept, and the one nothing references is deleted — in the
/// same transaction, on the same pass.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_referenced_terminal_event_survives_the_sweep() -> Result<()> {
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    async {
        let workspace_id = workspace(&db).await?;
        let referenced = terminal_event(&db, workspace_id, "crowdrelay.contact.attempted").await?;
        let plain = terminal_event(&db, workspace_id, "crowdrelay.fan.welcomed").await?;
        action_emission(&db, workspace_id, referenced).await?;

        let stats = worker(&db)?.run_once().await?;
        ensure!(stats.terminal_outbox_events_deleted == 1);

        let remaining: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM outbox_events WHERE workspace_id = $1")
                .bind(workspace_id)
                .fetch_all(&db)
                .await?;
        ensure!(remaining == vec![referenced], "remaining: {remaining:?}");
        let _ = plain;
        Ok(())
    }
    .await
}
