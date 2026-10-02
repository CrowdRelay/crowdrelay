//! Measured owned-social yield must control the next social channel.
//!
//! The Brain pins the winner into the dispatch decision. The LLM may write
//! the copy, but it may not silently substitute another platform and break
//! the acquisition feedback loop.

use crate::common;
use anyhow::{Context, Result, ensure};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_worker::agent_outcomes::AgentOutcomeWorker;
use serde_json::json;
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

async fn workspace(pool: &PgPool) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1,$2,$3)")
        .bind(id)
        .bind(format!("owned-social-channel-{}", id.simple()))
        .bind("Owned Social Channel")
        .execute(pool)
        .await?;
    Ok(WorkspaceId::from_uuid(id))
}

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
    .await?;
    Ok(())
}

async fn pinned_task(pool: &PgPool, workspace_id: WorkspaceId, platform: &str) -> Result<Uuid> {
    create_foreign_task_table(pool).await?;
    let decision_id = Uuid::now_v7();
    let action_id = Uuid::now_v7();
    let task_id = Uuid::now_v7();

    sqlx::query(
        r#"
        INSERT INTO autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id
        ) VALUES (
            $1,$2,$3,'growth_intelligence','workspace',$2,
            'request_agent_run',10000,'auto_execute','repeat measured social winner',
            jsonb_build_object('selected_social_platform',$4::text),
            '{}'::jsonb,'{}'::jsonb,$5
        )
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("social-channel-decision-{decision_id}"))
    .bind(platform)
    .bind(task_id)
    .execute(pool)
    .await
    .context("insert pinned dispatch decision")?;

    sqlx::query(
        r#"
        INSERT INTO autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, finished_at, trace_id
        ) VALUES (
            $1,$2,$3,'growth_intelligence','agent.run','workspace',$2,
            $4,$5,'succeeded',now(),$6
        )
        "#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(format!("social-channel-action-{action_id}"))
    .bind(json!({
        "kind": "request_agent_run",
        "template_id": "social-post",
        "prompt": format!("TARGET PLATFORM: {platform}"),
        "priority": 2,
        "tier": "premium"
    }))
    .bind(task_id)
    .execute(pool)
    .await
    .context("insert dispatch action")?;

    sqlx::query(
        r#"
        INSERT INTO agent_service_tasks
            (id, workspace_id, template_id, model_id, prompt, status, tier, metadata)
        VALUES ($1,$2,'social-post','auto',$3,'succeeded','premium',$4)
        "#,
    )
    .bind(task_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("TARGET PLATFORM: {platform}"))
    .bind(json!({"source":"autopilot","action_id":action_id}))
    .execute(pool)
    .await
    .context("insert foreign task")?;

    Ok(task_id)
}

async fn outcome(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    task_id: Uuid,
    platform: &str,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO agent_outcomes (
            id, workspace_id, task_id, result_id, kind, schema_version,
            payload, confidence_basis_points, idempotency_key, status
        ) VALUES ($1,$2,$3,$4,'social_post',1,$5,8000,$6,'pending')
        "#,
    )
    .bind(id)
    .bind(workspace_id.into_uuid())
    .bind(task_id)
    .bind(Uuid::now_v7())
    .bind(json!({
        "item": {
            "platform": platform,
            "text": "nowy numer już jest"
        },
        "rationale": "owned social draft",
        "provenance": {
            "verification": { "status": "grounding_check_passed" },
            "context": { "any_source_failed": false, "any_source_truncated": false },
            "confidence": {
                "basis_points": 8000,
                "source": "model_self_report",
                "is_evidence_confidence": false
            },
            "model": { "actual": "test-model", "provider": "test" }
        }
    }))
    .bind(format!("owned-social-outcome-{id}"))
    .execute(pool)
    .await?;
    Ok(id)
}

fn worker(pool: &PgPool, workspace_id: WorkspaceId) -> AgentOutcomeWorker {
    AgentOutcomeWorker::new(
        pool.clone(),
        workspace_id,
        Duration::from_secs(60),
        Duration::from_secs(30),
        "https://virya.music".to_owned(),
        crowdrelay_worker::auto_post_platforms::AutoPostPlatforms::default(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_model_cannot_substitute_the_measured_social_winner() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool).await?;
    let task_id = pinned_task(&pool, ws, "facebook").await?;
    let outcome_id = outcome(&pool, ws, task_id, "instagram").await?;

    worker(&pool, ws).run_once().await?;

    let (status, reason): (String, Option<String>) =
        sqlx::query_as("SELECT status,rejection_reason FROM agent_outcomes WHERE id=$1")
            .bind(outcome_id)
            .fetch_one(&pool)
            .await?;
    ensure!(
        status == "rejected",
        "mismatched draft must be rejected: {status}"
    );
    ensure!(
        reason
            .as_deref()
            .is_some_and(|r| r.contains("SOCIAL_PLATFORM_MISMATCH")),
        "rejection must name the channel contract: {reason:?}"
    );
    let outward: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM autopilot_actions WHERE workspace_id=$1 AND subject_kind='agent_outcome'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(outward == 0, "mismatch must create no outward action");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_measured_social_winner_flows_through_normally() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool).await?;
    let task_id = pinned_task(&pool, ws, "facebook").await?;
    outcome(&pool, ws, task_id, "facebook").await?;

    let processed = worker(&pool, ws).run_once().await?;
    ensure!(processed == 1, "matching outcome must process");
    let outward: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM autopilot_actions WHERE workspace_id=$1 AND subject_kind='agent_outcome'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(
        outward == 1,
        "matching draft must reach the normal approval action"
    );
    Ok(())
}
