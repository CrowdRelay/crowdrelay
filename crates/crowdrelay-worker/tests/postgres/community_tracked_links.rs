//! The tracked-link seam between a community draft and `smart_links`.
//!
//! A draft that names no `smart_link` still posts tracked — the minted
//! `/l/agent-*` wraps the registered source's canonical URL, and the click
//! becomes attribution the causal model can learn from. The guardrails
//! around it matter as much as the link: a label that violates the
//! `channel_community` CHECK must degrade to NULL, not abort the outcome's
//! whole transaction.

use crate::common;
use crate::community_relay_batch::{
    action_payload, action_rows, community_target, content_source, engage_outcome, worker,
    workspace,
};

use anyhow::{Result, ensure};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_draft_without_a_smart_link_still_posts_tracked() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let source_id = content_source(&pool, ws).await?;
    let target_a = community_target(&pool, ws, "metalpolska").await?;
    engage_outcome(&pool, ws, target_a, source_id, "metalpolska").await?;
    worker(&pool, ws).run_once().await?;

    let action_id = action_rows(&pool, ws).await?[0].0;
    let payload = action_payload(&pool, action_id).await?;
    let smart_link = payload["smart_link"].as_str().unwrap_or_default();
    ensure!(
        smart_link.starts_with("/l/agent-"),
        "the draft with no proposal still carries a tracked link, got {smart_link:?}"
    );
    // And it wraps the registered source's own URL — never a model guess.
    let destination = sqlx::query_scalar::<_, String>(
        "SELECT destination_url FROM smart_links \
         WHERE workspace_id = $1 AND slug = $2",
    )
    .bind(ws.into_uuid())
    .bind(smart_link.trim_start_matches("/l/"))
    .fetch_one(&pool)
    .await?;
    ensure!(
        destination == "https://reddit.com/r/band/comments/abc",
        "the tracked link answers the canonical source, got {destination}"
    );
    Ok(())
}

/// A `subreddit` label that violates the `channel_community` CHECK
/// (non-blank, <=120 chars) used to abort the whole outcome transaction —
/// the INSERT error poisons the tx and every later statement fails with
/// 25P02. The label is normalized to NULL instead, the link still mints,
/// and the draft still maps.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_unbounded_subreddit_label_never_aborts_the_outcome() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let source_id = content_source(&pool, ws).await?;
    let target = community_target(&pool, ws, "metalpolska").await?;
    // 200 chars of whitespace-framed garbage — impossible on Reddit,
    // perfectly possible from a model.
    engage_outcome(
        &pool,
        ws,
        target,
        source_id,
        &format!("  {}  ", "x".repeat(200)),
    )
    .await?;
    worker(&pool, ws).run_once().await?;

    let actions = action_rows(&pool, ws).await?;
    ensure!(!actions.is_empty(), "the outcome still produced an action");
    let payload = action_payload(&pool, actions[0].0).await?;
    let smart_link = payload["smart_link"].as_str().unwrap_or_default();
    ensure!(
        smart_link.starts_with("/l/agent-"),
        "the link still minted, got {smart_link:?}"
    );
    let channel_community = sqlx::query_scalar::<_, Option<String>>(
        "SELECT channel_community FROM smart_links \
         WHERE workspace_id = $1 AND slug = $2",
    )
    .bind(ws.into_uuid())
    .bind(smart_link.trim_start_matches("/l/"))
    .fetch_one(&pool)
    .await?;
    ensure!(
        channel_community.is_none(),
        "the offending label was dropped, got {channel_community:?}"
    );
    Ok(())
}
