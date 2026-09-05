//! Regression tests for the brain decision pipeline data-quality guards.
//!
//! NO EVIDENCE = NO OPPORTUNITY. A connector failure (Reddit credential
//! error) produces 0 confidence, 0 evidence, and "Unnamed target" — and
//! without the guards in `map_outcome`, that still became a decision with
//! an `awaiting_approval` action. These tests drive a real ingestion cycle
//! and assert the outcome is rejected (no decision, no action) for each
//! guard condition.
//!
//! They also prove the positive case: a valid outcome with evidence and a
//! real target identity flows through to a decision + action normally.

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
        let name = format!("crowdrelay_guards_{}", Uuid::now_v7().simple());
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

async fn workspace(pool: &PgPool) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("guards-{}", id.simple()))
        .bind("Guards Test")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(WorkspaceId::from_uuid(id))
}

/// Inserts an outcome row directly, as the agents service would.
async fn insert_outcome(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    kind: &str,
    confidence: i32,
    payload: serde_json::Value,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO agent_outcomes (
            id, workspace_id, task_id, result_id, kind, schema_version,
            payload, confidence_basis_points, idempotency_key, status
        ) VALUES ($1,$2,$3,$4,$5,1,$6,$7,$8,'pending')
        "#,
    )
    .bind(id)
    .bind(workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .bind(kind)
    .bind(&payload)
    .bind(confidence)
    .bind(format!("guards-test-{id}"))
    .execute(pool)
    .await
    .context("insert outcome")?;
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

/// Counts decisions for a workspace that came from agent outcomes.
async fn decision_count(pool: &PgPool, workspace_id: WorkspaceId) -> Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM viryaos_autopilot_decisions \
         WHERE workspace_id = $1 AND subject_kind = 'agent_outcome'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(pool)
    .await?)
}

/// Counts actions for a workspace that came from agent outcomes.
async fn action_count(pool: &PgPool, workspace_id: WorkspaceId) -> Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM viryaos_autopilot_actions \
         WHERE workspace_id = $1 AND subject_kind = 'agent_outcome'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(pool)
    .await?)
}

/// Returns the rejection_reason of the outcome, if rejected.
async fn rejection_reason(pool: &PgPool, outcome_id: Uuid) -> Result<Option<String>> {
    Ok(sqlx::query_scalar::<_, Option<String>>(
        "SELECT rejection_reason FROM agent_outcomes WHERE id = $1",
    )
    .bind(outcome_id)
    .fetch_one(pool)
    .await?)
}

// ── Guard: zero confidence → zero decisions ──────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn zero_confidence_outreach_target_produces_zero_decisions() -> Result<()> {
    let database = DisposableDatabase::create().await?;
    let result = zero_confidence_inner(&database.pool).await;
    database.drop_database().await;
    result
}

async fn zero_confidence_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    let outcome_id = insert_outcome(
        pool,
        ws,
        "outreach_targets",
        0,
        json!({
            "item": {
                "target_kind": "creator",
                "display_name": "r/metalpolska",
                "evidence_urls": ["https://reddit.com/r/metalpolska"],
            },
            "rationale": "test",
        }),
    )
    .await?;

    worker(pool, ws).run_once().await?;

    ensure!(
        decision_count(pool, ws).await? == 0,
        "zero-confidence outcome must not create a decision"
    );
    ensure!(
        action_count(pool, ws).await? == 0,
        "zero-confidence outcome must not create an action"
    );
    let reason = rejection_reason(pool, outcome_id).await?;
    ensure!(
        reason
            .as_ref()
            .is_some_and(|r| r.contains("INSUFFICIENT_EVIDENCE")),
        "rejection reason must mention INSUFFICIENT_EVIDENCE, got {reason:?}"
    );
    Ok(())
}

// ── Guard: zero evidence → zero decisions ────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn zero_evidence_outreach_target_produces_zero_decisions() -> Result<()> {
    let database = DisposableDatabase::create().await?;
    let result = zero_evidence_inner(&database.pool).await;
    database.drop_database().await;
    result
}

async fn zero_evidence_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    let outcome_id = insert_outcome(
        pool,
        ws,
        "outreach_targets",
        5000,
        json!({
            "item": {
                "target_kind": "creator",
                "display_name": "r/metalpolska",
                "evidence_urls": [],
            },
            "rationale": "test",
        }),
    )
    .await?;

    worker(pool, ws).run_once().await?;

    ensure!(
        decision_count(pool, ws).await? == 0,
        "zero-evidence outcome must not create a decision"
    );
    ensure!(
        action_count(pool, ws).await? == 0,
        "zero-evidence outcome must not create an action"
    );
    let reason = rejection_reason(pool, outcome_id).await?;
    ensure!(
        reason
            .as_ref()
            .is_some_and(|r| r.contains("INSUFFICIENT_EVIDENCE")),
        "rejection reason must mention INSUFFICIENT_EVIDENCE, got {reason:?}"
    );
    Ok(())
}

// ── Guard: unnamed target → zero decisions ───────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn unnamed_target_produces_zero_decisions() -> Result<()> {
    let database = DisposableDatabase::create().await?;
    let result = unnamed_inner(&database.pool).await;
    database.drop_database().await;
    result
}

async fn unnamed_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    let outcome_id = insert_outcome(
        pool,
        ws,
        "outreach_targets",
        5000,
        json!({
            "item": {
                "target_kind": "creator",
                "display_name": "Unnamed target",
                "evidence_urls": ["https://reddit.com/r/test"],
            },
            "rationale": "test",
        }),
    )
    .await?;

    worker(pool, ws).run_once().await?;

    ensure!(
        decision_count(pool, ws).await? == 0,
        "unnamed-target outcome must not create a decision"
    );
    ensure!(
        action_count(pool, ws).await? == 0,
        "unnamed-target outcome must not create an action"
    );
    let reason = rejection_reason(pool, outcome_id).await?;
    ensure!(
        reason
            .as_ref()
            .is_some_and(|r| r.contains("MISSING_TARGET_IDENTITY")),
        "rejection reason must mention MISSING_TARGET_IDENTITY, got {reason:?}"
    );
    Ok(())
}

// ── Positive: valid outcome → normal flow ────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn valid_outreach_target_produces_normal_flow() -> Result<()> {
    let database = DisposableDatabase::create().await?;
    let result = valid_inner(&database.pool).await;
    database.drop_database().await;
    result
}

async fn valid_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    insert_outcome(
        pool,
        ws,
        "outreach_targets",
        5000,
        json!({
            "item": {
                "target_kind": "creator",
                "display_name": "r/metalpolska",
                "evidence_urls": ["https://reddit.com/r/metalpolska"],
                "why_fit": "active metal community",
            },
            "rationale": "found via Reddit search",
        }),
    )
    .await?;

    let processed = worker(pool, ws).run_once().await?;
    ensure!(
        processed == 1,
        "valid outcome must be processed, got {processed}"
    );
    ensure!(
        decision_count(pool, ws).await? == 1,
        "valid outcome must create exactly one decision"
    );
    ensure!(
        action_count(pool, ws).await? == 1,
        "valid outreach target must create exactly one action"
    );
    Ok(())
}

// ── Positive: zero-confidence insight still passes (recommend_only) ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn zero_confidence_insight_still_creates_decision() -> Result<()> {
    let database = DisposableDatabase::create().await?;
    let result = insight_inner(&database.pool).await;
    database.drop_database().await;
    result
}

async fn insight_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    insert_outcome(
        pool,
        ws,
        "generic_insight",
        0,
        json!({
            "rationale": "weak observation but still an observation",
            "kind": "generic_insight",
        }),
    )
    .await?;

    let processed = worker(pool, ws).run_once().await?;
    ensure!(
        processed == 1,
        "zero-confidence insight must still be processed, got {processed}"
    );
    ensure!(
        decision_count(pool, ws).await? == 1,
        "zero-confidence insight must create a decision (it's an observation, not an action)"
    );
    // Insights are recommend_only — no action.
    ensure!(
        action_count(pool, ws).await? == 0,
        "insight must not create an action (recommend_only)"
    );
    Ok(())
}
