//! Regression tests for the receipt-reconciliation closure property.
//!
//! A `succeeded` action that was emitted but never received an executor
//! report is marked `unknown` by the gap sweep, then resolved from
//! delivery evidence. Before the synthesized receipt, resolution wrote
//! `finished_at = now()` and *no* execution report — so the action stayed
//! a gap candidate and re-entered `unknown` every 24h forever. That is
//! the loop the prod `awaiting_executor` count could never drain. The
//! fix writes a `receipt_reconciliation` report on resolve; these tests
//! prove a resolved action closes instead of looping.

use std::time::Duration;

use anyhow::{Context, Result};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_worker::receipt_reconciliation::ReceiptReconciliationWorker;
use serde_json::json;
use sqlx::{Connection, PgConnection, PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

struct DisposableDatabase {
    admin_url: String,
    name: String,
    pool: PgPool,
}

impl DisposableDatabase {
    async fn create() -> Result<Self> {
        let base_url = std::env::var("CROWDRELAY_TEST_DATABASE_URL")
            .context("CROWDRELAY_TEST_DATABASE_URL must target a disposable database")?;
        let (prefix, _) = base_url
            .rsplit_once('/')
            .context("database URL has no database name")?;
        let admin_url = format!("{prefix}/postgres");
        let name = format!("crowdrelay_receipt_{}", Uuid::now_v7().simple());
        let mut admin = PgConnection::connect(&admin_url)
            .await
            .context("connect to the maintenance database")?;
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await
            .context("create the disposable database")?;
        drop(admin);
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&format!("{prefix}/{name}"))
            .await
            .context("connect to the disposable database")?;
        crowdrelay_infra::database::MIGRATOR
            .run(&pool)
            .await
            .context("apply migrations")?;
        Ok(Self {
            admin_url,
            name,
            pool,
        })
    }

    async fn drop_database(self) {
        self.pool.close().await;
        if let Ok(mut admin) = PgConnection::connect(&self.admin_url).await {
            let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {} (FORCE)", self.name))
                .execute(&mut admin)
                .await;
        }
    }
}

fn worker(pool: PgPool, workspace_id: WorkspaceId) -> ReceiptReconciliationWorker {
    ReceiptReconciliationWorker::new(
        pool,
        workspace_id,
        Duration::from_secs(60),
        Duration::from_secs(30),
    )
}

async fn workspace(pool: &PgPool) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("receipt-{}", id.simple()))
        .bind("Receipt")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(WorkspaceId::from_uuid(id))
}

/// A `succeeded` action that was emitted to an executor two days ago and
/// never received a terminal report — the exact shape of the prod backlog.
/// Its outbox event was delivered, which is the evidence the resolver
/// uses to close it.
async fn emitted_action_without_receipt(pool: &PgPool, workspace_id: WorkspaceId) -> Result<Uuid> {
    let decision_id = Uuid::now_v7();
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id
        ) VALUES ($1,$2,$3,'outreach_supply','workspace',$4,
                  'outreach.discovery',9000,'auto_execute','discover outreach targets',
                  '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("dispatch-{decision_id}"))
    .bind(workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert dispatching decision")?;
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, finished_at, trace_id
        ) VALUES ($1,$2,$3,'outreach_supply','outreach.discovery.request','workspace',
                  $4,$5,$6,'succeeded',now() - interval '2 days',$7)
        "#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("action-{action_id}"))
    .bind(json!({"kind":"request_outreach_discovery","requested_candidates":5}))
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert emitted action")?;
    let outbox_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO outbox_events
            (id, workspace_id, event_type, payload, status, action_id, delivered_at)
        VALUES ($1,$2,'crowdrelay.outreach.discovery_requested','{}'::jsonb,
                'delivered',$3,now() - interval '2 days')
        "#,
    )
    .bind(outbox_id)
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .execute(pool)
    .await
    .context("insert delivered outbox event")?;
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_action_emissions
            (workspace_id, action_id, emission_key, outbox_event_id)
        VALUES ($1,$2,$3,$4)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(format!("emit-{action_id}"))
    .bind(outbox_id)
    .execute(pool)
    .await
    .context("insert emission")?;
    Ok(action_id)
}

async fn action_status(pool: &PgPool, action_id: Uuid) -> Result<String> {
    sqlx::query_scalar("SELECT status FROM viryaos_autopilot_actions WHERE id = $1")
        .bind(action_id)
        .fetch_one(pool)
        .await
        .context("read action status")
}

/// The resolution must write the receipt the gap sweep looks for, or the
/// action re-enters `unknown` every 24h forever — the loop prod was in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_resolved_action_gets_a_synthesized_receipt_and_stays_closed() -> Result<()> {
    let db = DisposableDatabase::create().await?;
    let result = async {
        let ws = workspace(&db.pool).await?;
        let action_id = emitted_action_without_receipt(&db.pool, ws).await?;
        let delivered_at: time::OffsetDateTime = sqlx::query_scalar(
            "SELECT delivered_at FROM outbox_events WHERE action_id = $1",
        )
        .bind(action_id)
        .fetch_one(&db.pool)
        .await?;

        // One cycle runs both sweeps in a transaction: gap → unknown,
        // outbox-delivered → resolved succeeded + synthesized receipt.
        worker(db.pool.clone(), ws).run_once().await?;
        assert_eq!(action_status(&db.pool, action_id).await?, "succeeded");

        let (executor_id, resolved_via, occurred_at): (String, String, time::OffsetDateTime) =
            sqlx::query_as(
                r#"
                SELECT executor_id, metadata->>'resolved_via', occurred_at
                FROM viryaos_autopilot_execution_reports
                WHERE action_id = $1 AND status = 'succeeded'
                "#,
            )
            .bind(action_id)
            .fetch_one(&db.pool)
            .await
            .context("read synthesized receipt")?;
        assert_eq!(executor_id, "receipt_reconciliation");
        assert_eq!(resolved_via, "outbox_delivery");
        // The receipt claims the evidence's own time — when the delivery
        // verifiably happened — not the moment the reconciler learned it.
        assert_eq!(occurred_at, delivered_at);

        // The closure property: age the action past the gap threshold
        // again and run a second cycle — it must NOT re-enter `unknown`.
        sqlx::query(
            "UPDATE viryaos_autopilot_actions SET finished_at = now() - interval '2 days' WHERE id = $1",
        )
        .bind(action_id)
        .execute(&db.pool)
        .await?;
        worker(db.pool.clone(), ws).run_once().await?;
        assert_eq!(action_status(&db.pool, action_id).await?, "succeeded");
        let report_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM viryaos_autopilot_execution_reports WHERE action_id = $1",
        )
        .bind(action_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(report_count, 1, "a re-resolution must not duplicate the receipt");
        Ok(())
    }
    .await;
    db.drop_database().await;
    result
}

/// When a real executor report already exists — the
/// `resolve_from_receipts` path — the synthesized insert must no-op:
/// two reports for one action would double-count confirmations.
///
/// The action must sit in `unknown` for the resolver to pick it up — an
/// `emitted` action with a report is skipped by every sweep, which is how a
/// previous version of this test passed without ever reaching the `NOT
/// EXISTS` guard it exists to cover.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_late_executor_report_is_not_duplicated() -> Result<()> {
    let db = DisposableDatabase::create().await?;
    let result = async {
        let ws = workspace(&db.pool).await?;
        let action_id = emitted_action_without_receipt(&db.pool, ws).await?;
        // The gap sweep already ran: the action sits in `unknown`, waiting on
        // evidence. The executor's report lands before the resolver runs.
        sqlx::query("UPDATE viryaos_autopilot_actions SET status = 'unknown' WHERE id = $1")
            .bind(action_id)
            .execute(&db.pool)
            .await
            .context("mark the action unknown")?;
        sqlx::query(
            r#"
            INSERT INTO viryaos_autopilot_execution_reports
                (id, workspace_id, action_id, receipt_key, executor_id, status, occurred_at)
            VALUES ($1,$2,$3,$4,'virya-n8n-primary','succeeded',now())
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(ws.into_uuid())
        .bind(action_id)
        .bind(format!("n8n-{action_id}"))
        .execute(&db.pool)
        .await
        .context("insert executor report")?;

        worker(db.pool.clone(), ws).run_once().await?;
        assert_eq!(action_status(&db.pool, action_id).await?, "succeeded");
        let report_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM viryaos_autopilot_execution_reports WHERE action_id = $1",
        )
        .bind(action_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(
            report_count, 1,
            "the real report must not gain a synthesized twin"
        );
        Ok(())
    }
    .await;
    db.drop_database().await;
    result
}
