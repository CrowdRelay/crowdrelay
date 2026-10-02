//! A community post must name a thread the band read in that room.
//!
//! The engager is handed the threads the community sweep recorded for the room
//! and must say which one its post sits next to (`fits_thread_url`). The worker
//! checks the claim against `fan_observations` for THIS target's room: a draft
//! that cites nothing, cites a thread the sweep never saw there, cites a thread
//! from another room, or lands after the room went quiet is rejected and leaves
//! no action. Only the repost template — which carries the band's own post and
//! is handed no threads — is exempt, and an unknown producer is not.

use crate::common;

use anyhow::{Context, Result, ensure};
use crowdrelay_domain::WorkspaceId;
use sqlx::PgPool;
use uuid::Uuid;

use super::community_relay_batch::{
    action_rows, community_target, content_source, engage_outcome, worker, workspace,
};

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

/// Points an outcome at a task of the given template, as the agents service
/// records which template produced a result.
async fn produced_by(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    outcome_id: Uuid,
    template: &str,
    target_id: Uuid,
) -> Result<()> {
    let task_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO agent_service_tasks (id, workspace_id, template_id, model_id, prompt)
         VALUES ($1,$2,$3,'test-model',$4)",
    )
    .bind(task_id)
    .bind(workspace_id.into_uuid())
    .bind(template)
    .bind(format!("target_id: {target_id}"))
    .execute(pool)
    .await?;
    sqlx::query("UPDATE agent_outcomes SET task_id = $2 WHERE id = $1")
        .bind(outcome_id)
        .bind(task_id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn cite(pool: &PgPool, outcome_id: Uuid, url: Option<&str>) -> Result<()> {
    match url {
        Some(url) => {
            sqlx::query(
                "UPDATE agent_outcomes
                 SET payload = jsonb_set(payload, '{item,fits_thread_url}', to_jsonb($2::text))
                 WHERE id = $1",
            )
            .bind(outcome_id)
            .bind(url)
            .execute(pool)
            .await?;
        }
        None => {
            sqlx::query(
                "UPDATE agent_outcomes SET payload = payload #- '{item,fits_thread_url}' WHERE id = $1",
            )
            .bind(outcome_id)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

async fn rejection(pool: &PgPool, outcome_id: Uuid) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT rejection_reason FROM agent_outcomes WHERE id = $1")
            .bind(outcome_id)
            .fetch_one(pool)
            .await?,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_post_must_name_a_thread_the_band_read_in_that_room() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    create_foreign_task_table(&pool).await?;
    let ws = workspace(&pool).await?;
    let source = content_source(&pool, ws).await?;
    let metal = community_target(&pool, ws, "metalroom").await?;
    // A second room, whose threads must not vouch for the first.
    let _jazz = community_target(&pool, ws, "jazzroom").await?;

    // 1. Cites a thread read in this room: becomes an approval-gated action.
    let good = engage_outcome(&pool, ws, metal, source, "metalroom").await?;
    produced_by(&pool, ws, good, "community-engager", metal).await?;
    // 2. Cites nothing.
    let silent = engage_outcome(&pool, ws, metal, source, "metalroom").await?;
    produced_by(&pool, ws, silent, "community-engager", metal).await?;
    cite(&pool, silent, None).await?;
    // 3. Invents a thread.
    let invented = engage_outcome(&pool, ws, metal, source, "metalroom").await?;
    produced_by(&pool, ws, invented, "community-engager", metal).await?;
    cite(
        &pool,
        invented,
        Some("https://www.reddit.com/comments/madeup9"),
    )
    .await?;
    // 4. Cites a real thread — from another room.
    let borrowed = engage_outcome(&pool, ws, metal, source, "metalroom").await?;
    produced_by(&pool, ws, borrowed, "community-engager", metal).await?;
    cite(
        &pool,
        borrowed,
        Some("https://www.reddit.com/comments/jazzroom1"),
    )
    .await?;
    // 5. No task row at all: an unknown producer is held to the gate too.
    let orphan = engage_outcome(&pool, ws, metal, source, "metalroom").await?;
    cite(&pool, orphan, None).await?;

    worker(&pool, ws).run_once().await.context("run outcomes")?;

    ensure!(
        rejection(&pool, good).await?.is_none(),
        "a post citing a thread read in its own room was refused: {:?}",
        rejection(&pool, good).await?
    );
    for (name, id) in [
        ("cites nothing", silent),
        ("invents a thread", invented),
        ("cites another room's thread", borrowed),
        ("unknown producer, cites nothing", orphan),
    ] {
        let reason = rejection(&pool, id).await?.unwrap_or_default();
        ensure!(
            reason.starts_with("UNREAD_ROOM"),
            "{name}: expected UNREAD_ROOM, got {reason:?}"
        );
    }
    let actions = action_rows(&pool, ws).await?;
    ensure!(
        actions.len() == 1,
        "exactly the one cited post may become an action, got {}",
        actions.len()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_room_that_went_quiet_before_the_answer_arrived_is_not_read() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    create_foreign_task_table(&pool).await?;
    let ws = workspace(&pool).await?;
    let source = content_source(&pool, ws).await?;
    let target = community_target(&pool, ws, "quietroom").await?;
    // The draft cites a real thread, but only two threads are still recent.
    sqlx::query("DELETE FROM fan_observations WHERE workspace_id = $1 AND url LIKE '%quietroom3'")
        .bind(ws.into_uuid())
        .execute(&pool)
        .await?;
    let outcome = engage_outcome(&pool, ws, target, source, "quietroom").await?;
    produced_by(&pool, ws, outcome, "community-engager", target).await?;

    worker(&pool, ws).run_once().await.context("run outcomes")?;

    let reason = rejection(&pool, outcome).await?.unwrap_or_default();
    ensure!(reason.starts_with("UNREAD_ROOM"), "got {reason:?}");
    ensure!(
        action_rows(&pool, ws).await?.is_empty(),
        "an action was created"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_repost_template_carries_the_bands_own_post_without_citing_a_thread() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    create_foreign_task_table(&pool).await?;
    let ws = workspace(&pool).await?;
    let source = content_source(&pool, ws).await?;
    let target = community_target(&pool, ws, "repostroom").await?;
    let outcome = engage_outcome(&pool, ws, target, source, "repostroom").await?;
    cite(&pool, outcome, None).await?;
    produced_by(&pool, ws, outcome, "community-repost", target).await?;

    worker(&pool, ws).run_once().await.context("run outcomes")?;

    // The repost template has its own source gate (a synced social post, not a
    // video), so this outcome may be refused for THAT reason; what it must not
    // be refused for is an unread room.
    let reason = rejection(&pool, outcome).await?.unwrap_or_default();
    ensure!(
        !reason.starts_with("UNREAD_ROOM"),
        "the repost template was held to the room gate: {reason:?}"
    );
    Ok(())
}
