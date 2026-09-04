//! An agent-produced decision must join to whatever caused it.
//!
//! `viryaos_autopilot_decisions` is the ledger an operator reads to answer "why
//! did the system do this?", and `trace_id` is the only column that joins a
//! decision to the event that produced it. The agents service is the only
//! writer of `agent_outcomes` and has never populated `trace_id`: all 67 rows in
//! production carried NULL, so all 42 decisions mapped from them were recorded
//! uncorrelated. The deterministic paths were traced correctly, which is why the
//! gap survived — half the recent decisions had a trace and nobody asked which
//! half.
//!
//! The correlation was never lost, only unrecorded. An outcome names its task,
//! and the task's metadata names the dispatching action, which holds the trace.
//! These tests drive a real ingestion cycle and assert the mapper walks that
//! chain, because the resolution is a `COALESCE` over a join and a unit test
//! with its own copy of the query would prove nothing about the one that ships.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_worker::agent_outcomes::AgentOutcomeWorker;
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
        let name = format!("crowdrelay_agent_trace_{}", Uuid::now_v7().simple());
        let mut admin = PgConnection::connect(&admin_url)
            .await
            .context("connect to the maintenance database")?;
        // The name is a fresh UUID, so there is nothing to quote-escape.
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

async fn workspace(pool: &PgPool) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("agent-trace-{}", id.simple()))
        .bind("Agent Trace")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(WorkspaceId::from_uuid(id))
}

/// A decision and the action it emitted, sharing one trace — the shape the
/// deterministic path writes.
async fn traced_action(pool: &PgPool, workspace_id: WorkspaceId, trace_id: Uuid) -> Result<Uuid> {
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
    .bind(trace_id)
    .execute(pool)
    .await
    .context("insert dispatching decision")?;
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, finished_at, trace_id
        ) VALUES ($1,$2,$3,'growth_intelligence','agent.run','workspace',
                  $4,$5,$6,'succeeded',now(),$7)
        "#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("action-{action_id}"))
    .bind(json!({"kind":"agent.run"}))
    .bind(trace_id)
    .execute(pool)
    .await
    .context("insert dispatching action")?;
    Ok(action_id)
}

/// `agent_service_tasks` belongs to the agents service — it is in
/// `FOREIGN_RELATIONS` and no CrowdRelay migration creates it, so a disposable
/// database built from `MIGRATOR` does not have it. The tests that exercise the
/// join create the columns the resolution reads; the one that proves ingestion
/// survives without the agents schema deliberately does not.
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

/// A queued agent task, optionally stamped with the trace at dispatch and
/// optionally naming the action that dispatched it.
async fn task(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    action_id: Option<Uuid>,
    stamped_trace: Option<Uuid>,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    let mut metadata = json!({ "source": "autopilot" });
    if let Some(action_id) = action_id {
        metadata["action_id"] = json!(action_id);
    }
    if let Some(trace_id) = stamped_trace {
        metadata["trace_id"] = json!(trace_id);
    }
    sqlx::query(
        r#"
        INSERT INTO agent_service_tasks
            (id, workspace_id, template_id, model_id, prompt, status, tier, metadata)
        VALUES ($1,$2,'community-engager','auto','probe','succeeded','basic',$3)
        "#,
    )
    .bind(id)
    .bind(workspace_id.into_uuid())
    .bind(metadata)
    .execute(pool)
    .await
    .context("insert agent task")?;
    Ok(id)
}

/// An outcome exactly as the agents service writes one: no `trace_id`.
async fn untraced_outcome(pool: &PgPool, workspace_id: WorkspaceId, task_id: Uuid) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO agent_outcomes (
            id, workspace_id, task_id, result_id, kind, schema_version,
            payload, confidence_basis_points, idempotency_key, status, trace_id
        ) VALUES ($1,$2,$3,$4,'generic_insight',1,$5,7000,$6,'pending',NULL)
        "#,
    )
    .bind(id)
    .bind(workspace_id.into_uuid())
    .bind(task_id)
    .bind(Uuid::now_v7())
    .bind(json!({ "rationale": "probe insight", "kind": "generic_insight" }))
    .bind(format!("agent-trace-probe-{id}"))
    .execute(pool)
    .await
    .context("insert agent outcome")?;
    Ok(id)
}

fn worker(pool: &PgPool, workspace_id: WorkspaceId) -> AgentOutcomeWorker {
    AgentOutcomeWorker::new(
        pool.clone(),
        workspace_id,
        Duration::from_secs(60),
        Duration::from_secs(30),
    )
}

/// The trace of every decision written for this workspace.
async fn decision_traces(pool: &PgPool, workspace_id: WorkspaceId) -> Result<Vec<Uuid>> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT trace_id FROM viryaos_autopilot_decisions \
         WHERE workspace_id = $1 AND subject_kind = 'agent_outcome'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .context("read decision traces")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_untraced_outcome_inherits_the_trace_stamped_on_its_task() -> Result<()> {
    let database = DisposableDatabase::create().await?;
    let result = stamped_inner(&database.pool).await;
    database.drop_database().await;
    result
}

async fn stamped_inner(pool: &PgPool) -> Result<()> {
    create_foreign_task_table(pool).await?;
    let workspace_id = workspace(pool).await?;
    let trace_id = Uuid::now_v7();
    let action_id = traced_action(pool, workspace_id, trace_id).await?;
    let task_id = task(pool, workspace_id, Some(action_id), Some(trace_id)).await?;
    untraced_outcome(pool, workspace_id, task_id).await?;

    let processed = worker(pool, workspace_id)
        .run_once()
        .await
        .map_err(|error| anyhow::anyhow!("ingestion cycle failed: {error}"))?;
    ensure!(
        processed == 1,
        "expected one outcome processed, got {processed}"
    );

    let traces = decision_traces(pool, workspace_id).await?;
    ensure!(
        traces == vec![trace_id],
        "the decision must carry the dispatching trace, got {traces:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_task_dispatched_before_the_stamp_still_resolves_through_its_action() -> Result<()> {
    let database = DisposableDatabase::create().await?;
    let result = unstamped_inner(&database.pool).await;
    database.drop_database().await;
    result
}

async fn unstamped_inner(pool: &PgPool) -> Result<()> {
    create_foreign_task_table(pool).await?;
    // Every task already queued when this shipped names its action and carries
    // no trace stamp. Those outcomes must still land correlated, or the repair
    // only applies to work dispatched after the deploy.
    let workspace_id = workspace(pool).await?;
    let trace_id = Uuid::now_v7();
    let action_id = traced_action(pool, workspace_id, trace_id).await?;
    let task_id = task(pool, workspace_id, Some(action_id), None).await?;
    untraced_outcome(pool, workspace_id, task_id).await?;

    worker(pool, workspace_id)
        .run_once()
        .await
        .map_err(|error| anyhow::anyhow!("ingestion cycle failed: {error}"))?;

    let traces = decision_traces(pool, workspace_id).await?;
    ensure!(
        traces == vec![trace_id],
        "the decision must resolve the trace through the action, got {traces:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_outcome_with_no_dispatching_action_is_rooted_at_its_task() -> Result<()> {
    let database = DisposableDatabase::create().await?;
    let result = rooted_inner(&database.pool).await;
    database.drop_database().await;
    result
}

async fn rooted_inner(pool: &PgPool) -> Result<()> {
    create_foreign_task_table(pool).await?;
    // Work the agents service scheduled on its own has no upstream action. It
    // still may not produce an orphan: the trace roots at the task, which says
    // "this outcome's history starts here" rather than borrowing a correlation
    // that never happened.
    let workspace_id = workspace(pool).await?;
    let task_id = task(pool, workspace_id, None, None).await?;
    untraced_outcome(pool, workspace_id, task_id).await?;

    worker(pool, workspace_id)
        .run_once()
        .await
        .map_err(|error| anyhow::anyhow!("ingestion cycle failed: {error}"))?;

    let traces = decision_traces(pool, workspace_id).await?;
    ensure!(
        traces == vec![task_id],
        "an outcome with no dispatching action must root at its task, got {traces:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn ingestion_survives_a_deployment_with_no_agents_schema() -> Result<()> {
    let database = DisposableDatabase::create().await?;
    let result = no_agents_schema_inner(&database.pool).await;
    database.drop_database().await;
    result
}

async fn no_agents_schema_inner(pool: &PgPool) -> Result<()> {
    // No `create_foreign_task_table` here on purpose. `agent_service_tasks`
    // belongs to the agents service, so a CrowdRelay deployment can legitimately
    // run without it — and this is where the trace enrichment nearly took the
    // ledger down with it. Resolving the trace originally ran inside the
    // mapping transaction, and a query against a missing relation is an error
    // that poisons the transaction it runs in, so every outcome would have
    // failed to map rather than merely failing to be enriched.
    let workspace_id = workspace(pool).await?;
    let task_id = Uuid::now_v7();
    untraced_outcome(pool, workspace_id, task_id).await?;

    let processed = worker(pool, workspace_id)
        .run_once()
        .await
        .map_err(|error| anyhow::anyhow!("ingestion cycle failed: {error}"))?;
    ensure!(
        processed == 1,
        "the outcome must still be ingested without the agents schema, got {processed}"
    );

    let traces = decision_traces(pool, workspace_id).await?;
    ensure!(
        traces == vec![task_id],
        "the decision must root at its task when the trace cannot be looked up, got {traces:?}"
    );
    Ok(())
}
