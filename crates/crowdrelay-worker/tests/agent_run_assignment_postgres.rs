//! Regression tests for the `agent.run.request` assignment lifecycle.
//!
//! `execute_agent_run` dispatches by inserting an `agent_service_tasks`
//! row in the same transaction that lands the action `succeeded` — no
//! executor receipt and no outbox event are ever filed, so before the
//! `resolve_agent_runs` sweep the experiment assignment sat `dispatched`
//! forever and the causal learner could never count the run as realized
//! treatment. These tests drive a real reconciliation cycle against a
//! disposable database and assert each task state maps to the honest
//! assignment outcome.

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
        let name = format!("crowdrelay_agentrun_{}", Uuid::now_v7().simple());
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
        .bind(format!("agent-run-{}", id.simple()))
        .bind("Agent Run")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(WorkspaceId::from_uuid(id))
}

/// A `agent.run.request` action already landed `succeeded` — the shape the
/// dispatch path commits, plus its `dispatched` treatment assignment.
async fn dispatched_agent_run(pool: &PgPool, workspace_id: WorkspaceId) -> Result<Uuid> {
    let decision_id = Uuid::now_v7();
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id
        ) VALUES ($1,$2,$3,'growth_intelligence','workspace',$4,
                  'agent.dispatch',9000,'auto_execute','dispatch an agent run',
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
        ) VALUES ($1,$2,$3,'growth_intelligence','agent.run.request','workspace',
                  $4,$5,$6,'succeeded',now(),$7)
        "#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("action-{action_id}"))
    .bind(json!({"kind":"agent.run"}))
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert dispatching action")?;
    // fk_assignment_experiment: every assignment references a persisted
    // design row.
    let experiment_uuid = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_experiment_designs
           (experiment_uuid, workspace_id, intervention_key, logical_cycle_key,
            unit_kind, holdout_probability, interference_policy)
           VALUES ($1,$2,'agent.run','cycle-1','target_community',0.0,'none')"#,
    )
    .bind(experiment_uuid)
    .bind(workspace_id.into_uuid())
    .execute(pool)
    .await
    .context("insert experiment design")?;
    sqlx::query(
        r#"INSERT INTO viryaos_experiment_assignments
           (workspace_id, id, experiment_uuid, unit_id, unit_kind,
            arm, intended_template_id, propensity, prediction, context, strategy,
            eligibility_criteria, selection_context, interference_policy,
            contamination_estimate, is_interference_controllable,
            experiment_status, execution_status, action_id)
           VALUES ($1,$2,$3,$4,'target_community','treatment','community-engager',0.5,
                   '{}'::jsonb,'{}'::jsonb,'discovery',
                   '{}'::jsonb,'{}'::jsonb,'none',0.0,false,
                   'active','dispatched',$5)"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .bind(experiment_uuid)
    .bind(Uuid::now_v7())
    .bind(action_id)
    .execute(pool)
    .await
    .context("insert dispatched assignment")?;
    Ok(action_id)
}

/// `agent_service_tasks` belongs to the agents service — no CrowdRelay
/// migration creates it — so the disposable database gets the columns the
/// sweep reads, same as `agent_decision_trace_postgres.rs` does.
async fn create_foreign_task_table(pool: &PgPool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS agent_service_tasks (
            id uuid PRIMARY KEY,
            workspace_id uuid NOT NULL,
            template_id text NOT NULL,
            model_id text NOT NULL,
            prompt text NOT NULL,
            status text NOT NULL DEFAULT 'queued',
            tier text NOT NULL DEFAULT 'basic',
            metadata jsonb NOT NULL DEFAULT '{}'::jsonb,
            created_at timestamptz NOT NULL DEFAULT now()
        )
        "#,
    )
    .execute(pool)
    .await
    .context("create the foreign agent task table")?;
    Ok(())
}

/// The task row `execute_agent_run` inserts — `metadata.action_id` is the
/// link the sweep resolves through.
async fn task(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    action_id: Uuid,
    status: &str,
    created_at: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO agent_service_tasks
            (id, workspace_id, template_id, model_id, prompt, status, tier, metadata, created_at)
        VALUES ($1,$2,'community-engager','auto','probe',$3,'basic',$4,
                now() - ($5)::interval)
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(status)
    .bind(json!({"source": "autopilot", "action_id": action_id}))
    .bind(created_at)
    .execute(pool)
    .await
    .context("insert agent task")?;
    Ok(())
}

async fn execution_status(pool: &PgPool, action_id: Uuid) -> Result<String> {
    sqlx::query_scalar(
        "SELECT execution_status FROM viryaos_experiment_assignments WHERE action_id = $1",
    )
    .bind(action_id)
    .fetch_one(pool)
    .await
    .context("read assignment execution_status")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn completed_task_marks_assignment_executed() -> Result<()> {
    let db = DisposableDatabase::create().await?;
    let result = async {
        let ws = workspace(&db.pool).await?;
        create_foreign_task_table(&db.pool).await?;
        let action_id = dispatched_agent_run(&db.pool, ws).await?;
        task(&db.pool, ws, action_id, "completed", "1 hour").await?;
        worker(db.pool.clone(), ws).run_once().await?;
        assert_eq!(execution_status(&db.pool, action_id).await?, "executed");
        Ok(())
    }
    .await;
    db.drop_database().await;
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn failed_task_marks_assignment_failed() -> Result<()> {
    let db = DisposableDatabase::create().await?;
    let result = async {
        let ws = workspace(&db.pool).await?;
        create_foreign_task_table(&db.pool).await?;
        let action_id = dispatched_agent_run(&db.pool, ws).await?;
        task(&db.pool, ws, action_id, "failed", "1 hour").await?;
        worker(db.pool.clone(), ws).run_once().await?;
        assert_eq!(execution_status(&db.pool, action_id).await?, "failed");
        Ok(())
    }
    .await;
    db.drop_database().await;
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn fresh_queued_task_stays_dispatched() -> Result<()> {
    let db = DisposableDatabase::create().await?;
    let result = async {
        let ws = workspace(&db.pool).await?;
        create_foreign_task_table(&db.pool).await?;
        let action_id = dispatched_agent_run(&db.pool, ws).await?;
        task(&db.pool, ws, action_id, "queued", "1 hour").await?;
        worker(db.pool.clone(), ws).run_once().await?;
        assert_eq!(execution_status(&db.pool, action_id).await?, "dispatched");
        Ok(())
    }
    .await;
    db.drop_database().await;
    result
}

/// A task still queued a day later belongs to a service that is down —
/// the intervention never happened and the assignment must stop waiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn stale_queued_task_is_reaped_failed() -> Result<()> {
    let db = DisposableDatabase::create().await?;
    let result = async {
        let ws = workspace(&db.pool).await?;
        create_foreign_task_table(&db.pool).await?;
        let action_id = dispatched_agent_run(&db.pool, ws).await?;
        task(&db.pool, ws, action_id, "queued", "2 days").await?;
        worker(db.pool.clone(), ws).run_once().await?;
        assert_eq!(execution_status(&db.pool, action_id).await?, "failed");
        Ok(())
    }
    .await;
    db.drop_database().await;
    result
}

/// `agent_service_tasks` belongs to the agents service. On a CrowdRelay-only
/// deployment the relation does not exist — a statement naming it would
/// error inside the sweep transaction and abort every other sweep with it.
/// The cycle must still run, and the assignment keeps waiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_missing_agents_schema_skips_the_sweep() -> Result<()> {
    let db = DisposableDatabase::create().await?;
    let result = async {
        let ws = workspace(&db.pool).await?;
        // Deliberately NO create_foreign_task_table call.
        let action_id = dispatched_agent_run(&db.pool, ws).await?;
        worker(db.pool.clone(), ws).run_once().await?;
        assert_eq!(execution_status(&db.pool, action_id).await?, "dispatched");
        Ok(())
    }
    .await;
    db.drop_database().await;
    result
}

/// The sweep only owns `agent.run.request` — a dispatched assignment for a
/// different action kind is someone else's job and must be left alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn other_action_kinds_are_untouched() -> Result<()> {
    let db = DisposableDatabase::create().await?;
    let result = async {
        let ws = workspace(&db.pool).await?;
        create_foreign_task_table(&db.pool).await?;
        let decision_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO viryaos_autopilot_decisions (
                id, workspace_id, decision_key, context, subject_kind, subject_id,
                decision_kind, confidence_basis_points, disposition, reason,
                input_snapshot, policy_snapshot, recommendation, trace_id
            ) VALUES ($1,$2,$3,'growth_intelligence','workspace',$4,
                      'community.engage',9000,'auto_execute','post to a community',
                      '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)
            "#,
        )
        .bind(decision_id)
        .bind(ws.into_uuid())
        .bind(format!("engage-{decision_id}"))
        .bind(ws.into_uuid())
        .bind(Uuid::now_v7())
        .execute(&db.pool)
        .await
        .context("insert engage decision")?;
        let action_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO viryaos_autopilot_actions (
                id, workspace_id, decision_id, context, action_kind, subject_kind,
                subject_id, idempotency_key, payload, status, finished_at
            ) VALUES ($1,$2,$3,'growth_intelligence','community.engage.request',
                      'workspace',$4,$5,'{}'::jsonb,'succeeded',now())
            "#,
        )
        .bind(action_id)
        .bind(ws.into_uuid())
        .bind(decision_id)
        .bind(ws.into_uuid())
        .bind(format!("action-{action_id}"))
        .execute(&db.pool)
        .await
        .context("insert community action")?;
        let experiment_uuid = Uuid::now_v7();
        sqlx::query(
            r#"INSERT INTO viryaos_experiment_designs
               (experiment_uuid, workspace_id, intervention_key, logical_cycle_key,
                unit_kind, holdout_probability, interference_policy)
               VALUES ($1,$2,'community.engage','cycle-2','target_community',0.0,'none')"#,
        )
        .bind(experiment_uuid)
        .bind(ws.into_uuid())
        .execute(&db.pool)
        .await
        .context("insert experiment design")?;
        sqlx::query(
            r#"INSERT INTO viryaos_experiment_assignments
               (workspace_id, id, experiment_uuid, unit_id, unit_kind,
                arm, intended_template_id, propensity, prediction, context, strategy,
                eligibility_criteria, selection_context, interference_policy,
                contamination_estimate, is_interference_controllable,
                experiment_status, execution_status, action_id)
               VALUES ($1,$2,$3,$4,'target_community','treatment','community-engager',0.5,
                       '{}'::jsonb,'{}'::jsonb,'discovery',
                       '{}'::jsonb,'{}'::jsonb,'none',0.0,false,
                       'active','dispatched',$5)"#,
        )
        .bind(ws.into_uuid())
        .bind(Uuid::now_v7())
        .bind(experiment_uuid)
        .bind(Uuid::now_v7())
        .bind(action_id)
        .execute(&db.pool)
        .await
        .context("insert dispatched assignment")?;
        worker(db.pool.clone(), ws).run_once().await?;
        assert_eq!(execution_status(&db.pool, action_id).await?, "dispatched");
        Ok(())
    }
    .await;
    db.drop_database().await;
    result
}
