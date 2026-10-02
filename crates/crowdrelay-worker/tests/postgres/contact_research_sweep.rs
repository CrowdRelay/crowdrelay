//! The sweep that sends the research agent after unread targets.
//!
//! The evaluator holds a target with no fact on file; this closes the loop. The
//! proofs are about who it picks and how much it spends: only open targets with
//! a live opportunity and no fact and no answer, most relevant first, within a
//! hard daily ceiling, never twice in a week, and never anybody it should not
//! read. It contacts nobody.

use crate::common;

use anyhow::{Context, Result, ensure};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_worker::contact_research_sweep::{ContactResearchSweep, PER_DAY, PER_PASS};
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

async fn workspace(pool: &PgPool) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("sweep-{}", id.simple()))
        .bind("Sweep Test")
        .execute(pool)
        .await
        .context("insert workspace")?;
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

/// A pitchable target and, when `relevance` is given, a live opportunity on it.
async fn target(
    pool: &PgPool,
    ws: WorkspaceId,
    email: &str,
    accepts: bool,
    reply: &str,
    relevance: Option<i32>,
) -> Result<Uuid> {
    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO outreach_targets
            (workspace_id, target_kind, display_name, contact_email, verified,
             accepts_outreach, last_reply_disposition)
         VALUES ($1, 'press', 'Redakcja', $2, true, $3, $4) RETURNING id",
    )
    .bind(ws.into_uuid())
    .bind(email)
    .bind(accepts)
    .bind(reply)
    .fetch_one(pool)
    .await?;
    if let Some(relevance) = relevance {
        sqlx::query(
            "INSERT INTO outreach_opportunities
                (workspace_id, target_id, source, subject_kind, subject_key, template_key,
                 relevance_basis_points, confidence_basis_points, observed_at, expires_at)
             VALUES ($1, $2, 'catalogue_autopilot', 'catalogue', 'echoes', 'outreach.press.v1',
                     $3, 9000, now() - interval '1 day', now() + interval '30 days')",
        )
        .bind(ws.into_uuid())
        .bind(id)
        .bind(relevance)
        .execute(pool)
        .await?;
    }
    Ok(id)
}

async fn researched_emails(pool: &PgPool, ws: WorkspaceId) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT metadata->>'subject_contact_email' FROM agent_service_tasks
         WHERE workspace_id = $1 AND template_id = 'contact-researcher' ORDER BY created_at, id",
    )
    .bind(ws.into_uuid())
    .fetch_all(pool)
    .await?)
}

fn sweep(pool: &PgPool, ws: WorkspaceId) -> ContactResearchSweep {
    ContactResearchSweep::new(pool.clone(), ws, Duration::from_secs(30))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn it_reads_only_the_people_the_engine_would_pitch() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    create_foreign_task_table(&pool).await?;
    let ws = workspace(&pool).await?;

    let wanted = target(
        &pool,
        ws,
        "pilne@gazeta.example.test",
        true,
        "none",
        Some(9500),
    )
    .await?;
    let _ = wanted;
    target(
        &pool,
        ws,
        "mniej@gazeta.example.test",
        true,
        "none",
        Some(8000),
    )
    .await?;
    // Not worth reading: no live opportunity, closed to outreach, already answered.
    target(
        &pool,
        ws,
        "bez-okazji@gazeta.example.test",
        true,
        "none",
        None,
    )
    .await?;
    target(
        &pool,
        ws,
        "zamkniete@gazeta.example.test",
        false,
        "none",
        Some(9900),
    )
    .await?;
    target(
        &pool,
        ws,
        "odpowiedzial@gazeta.example.test",
        true,
        "positive",
        Some(9900),
    )
    .await?;
    target(
        &pool,
        ws,
        "odmowil@gazeta.example.test",
        true,
        "declined",
        Some(9900),
    )
    .await?;
    // Already read.
    target(
        &pool,
        ws,
        "przeczytany@gazeta.example.test",
        true,
        "none",
        Some(9900),
    )
    .await?;
    sqlx::query(
        "INSERT INTO contact_research
            (workspace_id, normalized_email, fact, source_url, observed_on, researched_by)
         VALUES ($1, 'przeczytany@gazeta.example.test',
                 'recenzja płyty „Szum” w audycji „Metalowy Wieczór”',
                 'https://example.test/a', (now() AT TIME ZONE 'UTC')::date - 5, 'test')",
    )
    .bind(ws.into_uuid())
    .execute(&pool)
    .await?;

    let queued = sweep(&pool, ws).run_once().await?;
    ensure!(
        queued == 2,
        "expected the two open, unread, live targets, got {queued}"
    );
    // Most relevant first.
    let emails = researched_emails(&pool, ws).await?;
    ensure!(
        emails == ["pilne@gazeta.example.test", "mniej@gazeta.example.test"],
        "wrong people or wrong order: {emails:?}"
    );

    // A second pass the same hour sends nobody again: researched this week.
    ensure!(
        sweep(&pool, ws).run_once().await? == 0,
        "the sweep re-sent a person inside the week"
    );

    // It contacted nobody.
    let actions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM autopilot_actions WHERE workspace_id = $1")
            .bind(ws.into_uuid())
            .fetch_one(&pool)
            .await?;
    ensure!(actions == 0, "the sweep created {actions} action(s)");
    Ok(())
}

/// A big backlog is worked down over days, not spent in an hour: per pass and
/// per day there is a hard ceiling.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_spend_has_a_ceiling_per_pass_and_per_day() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    create_foreign_task_table(&pool).await?;
    let ws = workspace(&pool).await?;
    let total = usize::try_from(PER_DAY).unwrap_or(15) + 10;
    for index in 0..total {
        target(
            &pool,
            ws,
            &format!("redakcja-{index}@gazeta.example.test"),
            true,
            "none",
            Some(9000 - i32::try_from(index).unwrap_or(0)),
        )
        .await?;
    }

    let per_pass = usize::try_from(PER_PASS).unwrap_or(5);
    ensure!(
        sweep(&pool, ws).run_once().await? == per_pass,
        "one pass exceeded its ceiling"
    );
    let mut sent = per_pass;
    // Keep sweeping: the day's ceiling stops it, whatever the backlog.
    for _ in 0..10 {
        sent += sweep(&pool, ws).run_once().await?;
    }
    ensure!(
        sent == usize::try_from(PER_DAY).unwrap_or(15),
        "the daily ceiling is {PER_DAY}, the sweep sent {sent}"
    );
    Ok(())
}
