//! Does a drafted post ever become a post artifact?
//!
//! This covers the one link in the growth loop with no evidence on either
//! side. `telegram_posts`, `social_posts` and `discord_posts` hold zero rows
//! in production, and no test anywhere referenced those tables — so nothing
//! showed whether the step worked, only that it had never run.
//!
//! It had never run for a reason upstream of here: every agent outcome was
//! being refused for want of a grounding check, so no `agent.content.request`
//! action was ever created for an executor to pick up. That is fixed
//! elsewhere. This proves the step waiting on the other side of it.
//!
//! The executors draft in manual mode, which is the default and what
//! production runs, so these make no network call.

use anyhow::{Context, Result, ensure};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_worker::social_post_executor::SocialPostExecutorWorker;
use serde_json::json;
use sqlx::{Connection, PgConnection, PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

struct DisposableDatabase {
    pool: PgPool,
    admin_url: String,
    name: String,
}

impl DisposableDatabase {
    async fn create() -> Result<Self> {
        let base = std::env::var("CROWDRELAY_TEST_DATABASE_URL")
            .context("CROWDRELAY_TEST_DATABASE_URL must target a disposable database")?;
        let name = format!("crowdrelay_pubartifact_{}", Uuid::now_v7().simple());
        let mut admin = PgConnection::connect(&base).await?;
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await?;
        drop(admin);
        let url = {
            let (head, _) = base.rsplit_once('/').context("database url has no path")?;
            format!("{head}/{name}")
        };
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await?;
        crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
        // `agent_service_tasks` belongs to the TypeScript agents service, not
        // to CrowdRelay's migrations — it is one of the `FOREIGN_RELATIONS` the
        // SQL identifier gate allows for exactly that reason. The executor
        // joins it, so the test has to stand it up. Mirrors the owning DDL in
        // `crowdrelay-agents/src/store/db.ts`; only the columns this join
        // touches are declared.
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS agent_service_tasks (
                 id           UUID PRIMARY KEY,
                 workspace_id UUID NOT NULL,
                 template_id  TEXT NOT NULL,
                 model_id     TEXT NOT NULL,
                 prompt       TEXT NOT NULL,
                 status       TEXT NOT NULL DEFAULT 'queued',
                 error        TEXT,
                 created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
                 started_at   TIMESTAMPTZ,
                 completed_at TIMESTAMPTZ,
                 metadata     JSONB NOT NULL DEFAULT '{}',
                 tier         TEXT NOT NULL DEFAULT 'basic'
               )"#,
        )
        .execute(&pool)
        .await?;
        Ok(Self {
            pool,
            admin_url: base,
            name,
        })
    }

    async fn drop_database(self) {
        let Self {
            pool,
            admin_url,
            name,
        } = self;
        pool.close().await;
        if let Ok(mut admin) = PgConnection::connect(&admin_url).await {
            let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                .execute(&mut admin)
                .await;
        }
    }
}

async fn workspace(pool: &PgPool) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("pubartifact-{}", id.simple()))
        .bind("Publication Artifact Test")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(WorkspaceId::from_uuid(id))
}

/// The shape the executor actually joins on.
///
/// `social_post_executor` matches a succeeded `agent.content.request` to the
/// `agent_service_tasks` row named by `payload->>'task_id'`, and requires that
/// task's `template_id` to be `social-post`. Building the rows by hand here
/// keeps the test honest about that contract instead of restating its SQL.
async fn seed_drafted_action(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    template_id: &str,
    platform: &str,
) -> Result<Uuid> {
    let task_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO agent_service_tasks
           (id, workspace_id, template_id, model_id, prompt, status, tier)
           VALUES ($1,$2,$3,'auto','draft a post','completed','basic')"#,
    )
    .bind(task_id)
    .bind(workspace_id.into_uuid())
    .bind(template_id)
    .execute(pool)
    .await
    .context("insert agent task")?;

    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id)
           VALUES ($1,$2,$3,'promotion_budget','event',$4,
                   'agent_content_proposal',9000,'auto_execute','test',
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)"#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("pubartifact-{decision_id}"))
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert decision")?;

    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO viryaos_autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, finished_at)
           VALUES ($1,$2,$3,'promotion_budget','agent.content.request','event',
                   $4,$5,$6,'succeeded',now())"#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .bind(format!("pubartifact-action-{action_id}"))
    .bind(json!({
        "kind": "request_agent_content",
        "task_id": task_id,
        "draft": { "platform": platform, "text": "new single out friday", "cta_url": "https://virya.music" },
    }))
    .execute(pool)
    .await
    .context("insert action")?;
    Ok(action_id)
}

fn executor(pool: &PgPool, workspace_id: WorkspaceId) -> SocialPostExecutorWorker {
    // manual_mode = true: draft only, no network. This is the production
    // default and the state every channel has actually been in.
    SocialPostExecutorWorker::new(
        pool.clone(),
        workspace_id,
        true,
        None,
        "https://virya.music".to_owned(),
    )
    .expect("build executor")
}

async fn social_post_rows(pool: &PgPool, action_id: Uuid) -> Result<Vec<(String, String)>> {
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT platform, status FROM social_posts WHERE action_id = $1",
    )
    .bind(action_id)
    .fetch_all(pool)
    .await
    .context("read social_posts")?;
    Ok(rows)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_drafted_social_post_becomes_an_artifact_awaiting_a_person() -> Result<()> {
    let database = DisposableDatabase::create().await?;
    let result = drafted_becomes_artifact(&database.pool).await;
    database.drop_database().await;
    result
}

async fn drafted_becomes_artifact(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    let action_id = seed_drafted_action(pool, ws, "social-post", "instagram").await?;

    executor(pool, ws).run_once().await?;

    let rows = social_post_rows(pool, action_id).await?;
    ensure!(
        rows.len() == 1,
        "a succeeded agent.content.request must produce exactly one artifact, got {rows:?}"
    );
    let (platform, status) = &rows[0];
    ensure!(platform == "instagram", "platform must survive the draft");
    // Manual mode is the whole point: the artifact exists and waits for a
    // person. `dispatch_reached_an_audience` reads exactly this to decide
    // whether an outcome may be measured, so the status is load-bearing.
    ensure!(
        status == "awaiting_manual_post",
        "a manual-mode draft must wait for a person, got {status:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn running_twice_does_not_double_draft() -> Result<()> {
    let database = DisposableDatabase::create().await?;
    let result = twice_is_idempotent(&database.pool).await;
    database.drop_database().await;
    result
}

/// The executor polls. A second pass must not post the same draft again —
/// an outbound duplicate is the one error a fan can see.
async fn twice_is_idempotent(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    let action_id = seed_drafted_action(pool, ws, "social-post", "facebook").await?;

    executor(pool, ws).run_once().await?;
    executor(pool, ws).run_once().await?;

    let rows = social_post_rows(pool, action_id).await?;
    ensure!(
        rows.len() == 1,
        "two passes must leave exactly one artifact, got {rows:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn another_templates_draft_is_not_claimed() -> Result<()> {
    let database = DisposableDatabase::create().await?;
    let result = wrong_template_ignored(&database.pool).await;
    database.drop_database().await;
    result
}

/// Each executor claims only its own template's drafts.
///
/// The three executors share `agent.content.request` and separate on the
/// agent task's `template_id`. If that predicate loosened, the social
/// executor would publish Telegram copy to Instagram — the drafts are not
/// interchangeable, they are written for different audiences and formats.
async fn wrong_template_ignored(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    let action_id = seed_drafted_action(pool, ws, "telegram-poster", "instagram").await?;

    executor(pool, ws).run_once().await?;

    let rows = social_post_rows(pool, action_id).await?;
    ensure!(
        rows.is_empty(),
        "the social executor must not claim a telegram-poster draft, got {rows:?}"
    );
    Ok(())
}
