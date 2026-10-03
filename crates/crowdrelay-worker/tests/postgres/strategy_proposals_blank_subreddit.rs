//! A blank `subreddit` on a non-community outcome item.
//!
//! Split out of `strategy_proposals.rs` (whose fixtures it shares) when that
//! file outgrew the source-size ratchet.

use super::strategy_proposals::{insert_outcome, worker, workspace};
use crate::common;
use anyhow::{Result, ensure};
use serde_json::json;

/// A whitespace-only `subreddit` on a non-community item used to violate the
/// column's `btrim(subreddit) <> ''` CHECK and sink the whole outcome. It now
/// normalizes to NULL — a blank string is not a subreddit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_whitespace_subreddit_normalizes_to_null() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;

    let outcome_id = insert_outcome(
        &pool,
        ws,
        "outreach_targets",
        json!({
            "item": {
                "type": "outreach_target",
                "target_kind": "press",
                "display_name": "Metal Zine Weekly",
                "subreddit": "   ",
                "contact_email": "tips@metalzine.example",
                "evidence_urls": ["https://metalzine.example/about"],
            },
            "rationale": "a press contact",
        }),
    )
    .await?;

    worker(&pool, ws).run_once().await?;

    let (status, rejection): (String, Option<String>) =
        sqlx::query_as("SELECT status, rejection_reason FROM agent_outcomes WHERE id = $1")
            .bind(outcome_id)
            .fetch_one(&pool)
            .await?;
    ensure!(
        status == "processed",
        "a blank subreddit must not sink the outcome — {status} {rejection:?}"
    );
    let stored: Option<Option<String>> = sqlx::query_scalar(
        "SELECT subreddit FROM agent_outreach_targets \
         WHERE workspace_id = $1 AND display_name = 'Metal Zine Weekly'",
    )
    .bind(ws.into_uuid())
    .fetch_optional(&pool)
    .await?;
    ensure!(
        stored.clone().flatten().is_none(),
        "a whitespace subreddit must store NULL, got {stored:?}"
    );
    Ok(())
}
