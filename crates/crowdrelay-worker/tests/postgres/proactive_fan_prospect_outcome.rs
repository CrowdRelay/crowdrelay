//! Deterministic proactive FAN SCOUT candidate -> canonical prospect, end to end.
//!
//! The model selects a candidate_ref; it never supplies the identity written to
//! fan_prospects. Identity/evidence come from task metadata produced by the
//! deterministic Bandcamp discovery tool. No fan and no outward action are
//! created by discovery.

use crate::common;
use anyhow::{Result, ensure};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_worker::agent_outcomes::AgentOutcomeWorker;
use serde_json::json;
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

const TEMPLATE: &str = "bandcamp-scanner";
const CANDIDATE_REF: &str = "bc_fixture_123";

async fn workspace(pool: &PgPool) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id,slug,name) VALUES ($1,$2,$3)")
        .bind(id)
        .bind(format!("fan-prospect-agent-{}", id.simple()))
        .bind("Fan Prospect Agent")
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

fn candidate(ref_id: &str) -> serde_json::Value {
    json!({
        "candidate_ref": ref_id,
        "platform": "bandcamp",
        "platform_user_id": "fan-123",
        "handle": "metal_kuba",
        "display_identity": "metal_kuba",
        "display_name": "Kuba",
        "profile_url": "https://bandcamp.com/metal_kuba",
        "source_ref": ref_id,
        "source_url": "https://exampleband.bandcamp.com/album/heavy",
        "observed_at": time::OffsetDateTime::now_utc().format(
            &time::format_description::well_known::Rfc3339
        ).expect("timestamp"),
        "evidence": "Public collector entry for Heavy by Example Band"
    })
}

async fn task(pool: &PgPool, ws: WorkspaceId, candidates: serde_json::Value) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO agent_service_tasks
         (id,workspace_id,template_id,model_id,prompt,status,tier,metadata)
         VALUES ($1,$2,$3,'auto','select grounded fan prospects','completed','basic',$4)",
    )
    .bind(id)
    .bind(ws.into_uuid())
    .bind(TEMPLATE)
    .bind(json!({"fan_scout_candidates": candidates}))
    .execute(pool)
    .await?;
    Ok(id)
}

async fn outcome(
    pool: &PgPool,
    ws: WorkspaceId,
    task_id: Uuid,
    candidate_ref: Option<&str>,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    let item = candidate_ref.map(|candidate_ref| {
        json!({
            "type":"fan_prospect",
            "candidate_ref":candidate_ref,
            "why_fit":"collects similar music"
        })
    });
    sqlx::query(
        "INSERT INTO agent_outcomes
         (id,workspace_id,task_id,result_id,kind,schema_version,payload,
          confidence_basis_points,idempotency_key,status)
         VALUES ($1,$2,$3,$4,'fan_prospects',1,$5,8000,$6,'pending')"
    )
    .bind(id)
    .bind(ws.into_uuid())
    .bind(task_id)
    .bind(Uuid::now_v7())
    .bind(json!({
        "item": item,
        "rationale":"selected from deterministic Bandcamp collector evidence",
        "provenance":{
            "verification":{"status":"grounding_check_passed"},
            "context":{"any_source_failed":false,"any_source_truncated":false},
            "confidence":{"basis_points":8000,"source":"model_self_report","is_evidence_confidence":false},
            "model":{"actual":"test-model","provider":"test"}
        }
    }))
    .bind(format!("fan-prospect-outcome-{id}"))
    .execute(pool)
    .await?;
    Ok(id)
}

fn worker(pool: &PgPool, ws: WorkspaceId) -> AgentOutcomeWorker {
    AgentOutcomeWorker::new(
        pool.clone(),
        ws,
        Duration::from_secs(60),
        Duration::from_secs(30),
        "https://virya.music".to_owned(),
        crowdrelay_worker::auto_post_platforms::AutoPostPlatforms::default(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn grounded_bandcamp_candidate_becomes_a_prospect_not_a_fan_or_action() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    create_foreign_task_table(&pool).await?;
    let ws = workspace(&pool).await?;
    let task_id = task(&pool, ws, json!([candidate(CANDIDATE_REF)])).await?;
    let outcome_id = outcome(&pool, ws, task_id, Some(CANDIDATE_REF)).await?;

    worker(&pool, ws).run_once().await?;

    let (status, reason): (String, Option<String>) =
        sqlx::query_as("SELECT status,rejection_reason FROM agent_outcomes WHERE id=$1")
            .bind(outcome_id)
            .fetch_one(&pool)
            .await?;
    ensure!(status == "processed", "got {status}: {reason:?}");

    let prospect: (String, String, String, i64) = sqlx::query_as(
        "SELECT p.platform,p.external_identity,o.observation_kind,
                o.confidence_basis_points::bigint
         FROM fan_prospects p
         JOIN fan_prospect_observations o
           ON o.workspace_id=p.workspace_id AND o.prospect_id=p.id
         WHERE p.workspace_id=$1",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(
        prospect.0 == "bandcamp" && prospect.1 == "metal_kuba",
        "{prospect:?}"
    );
    ensure!(prospect.2 == "collects_similar_music", "{prospect:?}");
    ensure!(
        prospect.3 == 10_000,
        "tool evidence, not model confidence: {prospect:?}"
    );

    let fans: i64 = sqlx::query_scalar("SELECT count(*) FROM fans WHERE workspace_id=$1")
        .bind(ws.into_uuid())
        .fetch_one(&pool)
        .await?;
    let actions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM autopilot_actions WHERE workspace_id=$1")
            .bind(ws.into_uuid())
            .fetch_one(&pool)
            .await?;
    ensure!(fans == 0, "public collector must not become a fan");
    ensure!(actions == 0, "discovery must not contact anyone");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn invented_candidate_ref_is_rejected_and_writes_no_person() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    create_foreign_task_table(&pool).await?;
    let ws = workspace(&pool).await?;
    let task_id = task(&pool, ws, json!([candidate(CANDIDATE_REF)])).await?;
    let outcome_id = outcome(&pool, ws, task_id, Some("bc_invented")).await?;

    worker(&pool, ws).run_once().await?;
    let (status, reason): (String, Option<String>) =
        sqlx::query_as("SELECT status,rejection_reason FROM agent_outcomes WHERE id=$1")
            .bind(outcome_id)
            .fetch_one(&pool)
            .await?;
    ensure!(status == "rejected", "got {status}");
    ensure!(
        reason
            .as_deref()
            .is_some_and(|r| r.contains("deterministic candidates")),
        "wrong rejection: {reason:?}"
    );
    let prospects: i64 =
        sqlx::query_scalar("SELECT count(*) FROM fan_prospects WHERE workspace_id=$1")
            .bind(ws.into_uuid())
            .fetch_one(&pool)
            .await?;
    ensure!(prospects == 0, "invented person reached the prospect spine");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn empty_proactive_scan_is_a_processed_observation_not_a_failure() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    create_foreign_task_table(&pool).await?;
    let ws = workspace(&pool).await?;
    let task_id = task(&pool, ws, json!([])).await?;
    let outcome_id = outcome(&pool, ws, task_id, None).await?;
    worker(&pool, ws).run_once().await?;
    let status: String = sqlx::query_scalar("SELECT status FROM agent_outcomes WHERE id=$1")
        .bind(outcome_id)
        .fetch_one(&pool)
        .await?;
    ensure!(
        status == "processed",
        "empty search should be an honest result"
    );
    Ok(())
}
