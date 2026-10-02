//! The research agent's result -> what the band knows about a person, end to end.
//!
//! The rule: nobody is written to as a stranger. CrowdRelay holds anybody
//! without a recent, sourced fact on file; the `contact-researcher` agent finds
//! one, and these tests drive the real ingestion cycle
//! (`AgentOutcomeWorker::run_once`) to prove the worker believes it only when it
//! can check every claim against what the model was actually shown:
//!
//! * a fact whose source and date match the evidence lands, and the person is
//!   no longer held for being unread;
//! * a source the model was never shown, a date that is not the one the tool
//!   recorded for the page, another person's id, or a different template are
//!   all rejected and leave nothing on file;
//! * an honest-empty answer ("nothing recent found") is processed, not an error;
//! * researching contacts nobody, so no action is ever created.

use crate::common;

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_worker::agent_outcomes::AgentOutcomeWorker;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

const TEMPLATE: &str = "contact-researcher";
const EMAIL: &str = "redakcja@radio.example.test";
const PAGE: &str = "https://radio.example.test/audycje/metalowy-wieczor";

async fn workspace(pool: &PgPool) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("research-{}", id.simple()))
        .bind("Research Test")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(WorkspaceId::from_uuid(id))
}

/// `agent_service_tasks` belongs to the agents service; the suite database
/// needs the columns the producing-task join reads.
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

/// What the context builder recorded about the research tool's page: the URL,
/// the tool, and the snippet around the URL, which carries `published_on`.
fn evidence(published_on: &str, tool: &str) -> serde_json::Value {
    json!({
        "version": 1,
        "urls": [{
            "url": PAGE,
            "tool": tool,
            "label": "Contact Research",
            "snippet": format!(
                "\"url\": \"{PAGE}\",\n      \"published_on\": \"{published_on}\",\n      \"recent\": true"
            ),
            "fetched_at": "2026-10-02T10:00:00Z"
        }],
        "contacts": []
    })
}

async fn task(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    template: &str,
    subject_contact_email: Option<&str>,
    evidence: serde_json::Value,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO agent_service_tasks
            (id, workspace_id, template_id, model_id, prompt, status, tier, metadata)
        VALUES ($1,$2,$3,'auto','research brief','completed','premium',$4)
        "#,
    )
    .bind(id)
    .bind(workspace_id.into_uuid())
    .bind(template)
    .bind(json!({ "subject_contact_email": subject_contact_email, "evidence": evidence }))
    .execute(pool)
    .await
    .context("insert research task")?;
    Ok(id)
}

async fn insert_outcome(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    task_id: Uuid,
    item: Option<serde_json::Value>,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO agent_outcomes (
            id, workspace_id, task_id, result_id, kind, schema_version,
            payload, confidence_basis_points, idempotency_key, status
        ) VALUES ($1,$2,$3,$4,'contact_research',1,$5,8000,$6,'pending')
        "#,
    )
    .bind(id)
    .bind(workspace_id.into_uuid())
    .bind(task_id)
    .bind(Uuid::now_v7())
    .bind(json!({
        "item": item,
        "rationale": "one recent, sourced fact about the pinned person",
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
    .bind(format!("research-test-{id}"))
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
        "https://virya.music".to_owned(),
        crowdrelay_worker::auto_post_platforms::AutoPostPlatforms::default(),
    )
}

async fn outcome_status(pool: &PgPool, outcome_id: Uuid) -> Result<(String, Option<String>)> {
    Ok(sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT status, rejection_reason FROM agent_outcomes WHERE id = $1",
    )
    .bind(outcome_id)
    .fetch_one(pool)
    .await?)
}

async fn facts_on_file(
    pool: &PgPool,
    workspace_id: WorkspaceId,
) -> Result<Vec<(String, String, String)>> {
    Ok(sqlx::query_as::<_, (String, String, String)>(
        "SELECT normalized_email, fact, researched_by FROM contact_research
         WHERE workspace_id = $1 ORDER BY researched_at",
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await?)
}

fn day(days_ago: i64) -> String {
    (time::OffsetDateTime::now_utc() - time::Duration::days(days_ago))
        .date()
        .to_string()
}

fn finding(observed_on: &str) -> serde_json::Value {
    json!({
        "type": "contact_research",
        "fact": "recenzja płyty „Szum” w audycji „Metalowy Wieczór”",
        "praise": "Rzadko ktoś omawia tę płytę tak konkretnie.",
        "source_url": PAGE,
        "observed_on": observed_on,
        "language": "pl"
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_checked_fact_lands_and_the_person_is_no_longer_unread() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    create_foreign_task_table(&pool).await?;
    let ws = workspace(&pool).await?;
    let published = day(12);
    let task_id = task(
        &pool,
        ws,
        TEMPLATE,
        Some(EMAIL),
        evidence(&published, "research_contact"),
    )
    .await?;
    let outcome_id = insert_outcome(&pool, ws, task_id, Some(finding(&published))).await?;

    worker(&pool, ws).run_once().await?;

    let (status, reason) = outcome_status(&pool, outcome_id).await?;
    ensure!(status == "processed", "got {status}: {reason:?}");
    let facts = facts_on_file(&pool, ws).await?;
    ensure!(facts.len() == 1, "expected one fact on file, got {facts:?}");
    ensure!(facts[0].0 == "redakcja@radio.example.test", "{facts:?}");
    ensure!(
        facts[0].2 == "agent:contact-researcher",
        "who found it must be recorded: {facts:?}"
    );

    // The person the band reads is no longer held for being unread.
    let hook = crowdrelay_infra::contact_research::latest_hook(
        &pool,
        ws.into_uuid(),
        EMAIL,
        time::OffsetDateTime::now_utc().date(),
    )
    .await?;
    ensure!(
        hook.is_some_and(|hook| hook.source_url == PAGE),
        "the fact is not readable by the gate"
    );

    // Researching a person contacts nobody: no approval card, no action.
    let actions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM autopilot_actions WHERE workspace_id = $1")
            .bind(ws.into_uuid())
            .fetch_one(&pool)
            .await?;
    ensure!(actions == 0, "research created {actions} action(s)");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_claim_the_worker_cannot_check_leaves_nothing_on_file() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    create_foreign_task_table(&pool).await?;
    let ws = workspace(&pool).await?;
    let published = day(12);

    // (task template, pinned address, evidence, the item the model emitted, why it must fail)
    let mut cases: Vec<(
        &str,
        Option<&str>,
        serde_json::Value,
        serde_json::Value,
        &str,
    )> = Vec::new();
    let ok_evidence = || evidence(&published, "research_contact");

    // The model cites a date generously: the tool recorded `published`.
    cases.push((
        TEMPLATE,
        Some(EMAIL),
        ok_evidence(),
        finding(&day(2)),
        "recorded for that page",
    ));
    // A page the model was never shown.
    let mut invented = finding(&published);
    invented["source_url"] = json!("https://radio.example.test/audycje/wymyslone");
    cases.push((
        TEMPLATE,
        Some(EMAIL),
        ok_evidence(),
        invented,
        "showed the model",
    ));
    // A task that was never pinned to anybody: there is no person to file against.
    cases.push((
        TEMPLATE,
        None,
        ok_evidence(),
        finding(&published),
        "not pinned",
    ));
    // A page a different tool returned.
    cases.push((
        TEMPLATE,
        Some(EMAIL),
        evidence(&published, "web_search"),
        finding(&published),
        "research tool",
    ));
    // The wrong template.
    cases.push((
        "growth-strategist",
        Some(EMAIL),
        ok_evidence(),
        finding(&published),
        "contact-researcher",
    ));
    // Not the band's voice.
    let mut hype = finding(&published);
    hype["praise"] = json!("Świetna robota!");
    cases.push((TEMPLATE, Some(EMAIL), ok_evidence(), hype, "exclamation"));
    // Correctly sourced and dated, but not recent.
    let old = day(300);
    cases.push((
        TEMPLATE,
        Some(EMAIL),
        evidence(&old, "research_contact"),
        finding(&old),
        "not 'lately'",
    ));

    for (template, pinned, evidence, item, expected) in cases {
        let task_id = task(&pool, ws, template, pinned, evidence).await?;
        let outcome_id = insert_outcome(&pool, ws, task_id, Some(item)).await?;
        worker(&pool, ws).run_once().await?;
        let (status, reason) = outcome_status(&pool, outcome_id).await?;
        ensure!(
            status == "rejected",
            "{expected}: should be rejected, got {status}"
        );
        ensure!(
            reason.as_deref().unwrap_or_default().contains(expected),
            "{expected}: rejected for the wrong reason: {reason:?}"
        );
    }
    let facts = facts_on_file(&pool, ws).await?;
    ensure!(
        facts.is_empty(),
        "an unchecked claim reached the file: {facts:?}"
    );
    Ok(())
}

/// "Nothing recent found" is the correct answer when the evidence is thin, and
/// it must not read as a failure that gets retried forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn finding_nothing_recent_is_an_answer_not_an_error() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    create_foreign_task_table(&pool).await?;
    let ws = workspace(&pool).await?;
    let task_id = task(
        &pool,
        ws,
        TEMPLATE,
        Some(EMAIL),
        evidence(&day(300), "research_contact"),
    )
    .await?;
    let outcome_id = insert_outcome(&pool, ws, task_id, None).await?;

    worker(&pool, ws).run_once().await?;

    let (status, reason) = outcome_status(&pool, outcome_id).await?;
    ensure!(
        status == "processed",
        "an empty finding must be processed, got {status}: {reason:?}"
    );
    ensure!(
        facts_on_file(&pool, ws).await?.is_empty(),
        "nothing was found, nothing may be filed"
    );
    Ok(())
}
