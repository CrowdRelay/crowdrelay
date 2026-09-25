//! A tenant stack without an agent service.
//!
//! `agent_service_tasks` is the agent service's table; a stack without that
//! service (the demo tenants in production) has none. The three post
//! executors joined it inside their claim transaction, so every cycle there
//! failed with `relation "agent_service_tasks" does not exist` — three
//! warnings a minute per stack — and the abort also discarded the claim of
//! posts that were already pending. On a freshly migrated database, which
//! is exactly such a stack, each executor now completes a cycle.

use crate::common;

use anyhow::{Context, Result, ensure};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::sensitive_response::SensitiveResponseKey;
use crowdrelay_worker::{
    discord_executor::DiscordExecutorWorker, social_post_executor::SocialPostExecutorWorker,
    telegram_executor::TelegramExecutorWorker,
};
use uuid::Uuid;

fn key() -> SensitiveResponseKey {
    SensitiveResponseKey::derive_from_secret(b"test-encryption-key")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_post_executors_run_without_an_agent_service() -> Result<()> {
    let database = common::isolated_database("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .context("create an isolated, migrated database")?;
    let result = run(&database.pool).await;
    database
        .drop()
        .await
        .context("drop the isolated database")?;
    result
}

async fn run(pool: &sqlx::PgPool) -> Result<()> {
    let absent: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('agent_service_tasks')::text")
            .fetch_one(pool)
            .await?;
    ensure!(
        absent.is_none(),
        "the premise: no migration creates the agent service's table"
    );
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'No agents')")
        .bind(id)
        .bind(format!("no-agents-{}", id.simple()))
        .execute(pool)
        .await?;
    let workspace_id = WorkspaceId::from_uuid(id);
    let origin = "https://example.test".to_owned();

    let social = SocialPostExecutorWorker::new(
        pool.clone(),
        workspace_id,
        true,
        None,
        origin.clone(),
        key(),
        false,
    )
    .context("build social executor")?;
    social
        .run_once()
        .await
        .context("social post executor cycle without agent_service_tasks")?;

    TelegramExecutorWorker::new(pool.clone(), workspace_id, true, key(), origin.clone())
        .context("build telegram executor")?
        .run_once()
        .await
        .context("telegram executor cycle without agent_service_tasks")?;

    DiscordExecutorWorker::new(pool.clone(), workspace_id, true, key(), origin)
        .context("build discord executor")?
        .run_once()
        .await
        .context("discord executor cycle without agent_service_tasks")?;
    Ok(())
}
