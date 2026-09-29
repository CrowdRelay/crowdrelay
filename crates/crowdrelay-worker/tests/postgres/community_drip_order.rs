//! A relay batch's deliveries go out best community first, not draft order:
//! the pick inside the claim sweep orders pending deliveries by
//! sqrt(members) × survival × self-promo allowance. Runs under
//! `just test-postgres` against `CROWDRELAY_TEST_DATABASE_URL`.

use crate::common;

use std::time::Duration;

use anyhow::{Context, Result};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_worker::community_executor::CommunityExecutorWorker;
use sqlx::PgPool;
use uuid::Uuid;

use super::community_relay_batch::{community_target, content_source, workspace};

/// A pending batch delivery for a community whose place carries the given
/// member count — the ordering reads `c2.place_id`, not the target.
async fn batch_delivery(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    source_id: Uuid,
    subreddit: &str,
    members: i32,
) -> Result<()> {
    let place_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO discovery_places \
             (id, workspace_id, place_kind, platform, name, url, member_count) \
         VALUES ($1,$2,'subreddit','reddit',$3,$4,$5)",
    )
    .bind(place_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("r/{subreddit}"))
    .bind(format!("https://www.reddit.com/r/{subreddit}"))
    .bind(members)
    .execute(pool)
    .await
    .context("insert discovery place")?;
    let target_id = community_target(pool, workspace_id, subreddit).await?;
    let decision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_decisions (
             id, workspace_id, decision_key, context, subject_kind, subject_id,
             decision_kind, confidence_basis_points, disposition, reason,
             input_snapshot, policy_snapshot, recommendation, trace_id
         ) VALUES ($1,$2,$3,'promotion_budget','target_community',$4,
                   'agent_content_proposal',7500,'require_approval','test',
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,gen_random_uuid())",
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("drip-order-{decision_id}"))
    .bind(target_id)
    .execute(pool)
    .await
    .context("insert decision")?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_actions (
             id, workspace_id, decision_id, context, action_kind, subject_kind,
             subject_id, idempotency_key, payload, status, finished_at
         ) VALUES ($1,$2,$3,'promotion_budget','community.engage.request',
                   'target_community',$4,$5,$6,'succeeded',now())",
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(target_id)
    .bind(format!("drip-order-{action_id}"))
    .bind(serde_json::json!({
        "kind": "request_community_engagement",
        "target_id": target_id,
        "platform": "reddit",
        "subreddit": subreddit,
        "title": "New video is out",
        "body": "what do you think",
        "source_id": source_id,
    }))
    .execute(pool)
    .await
    .context("insert engage action")?;
    sqlx::query(
        "INSERT INTO community_posts \
             (workspace_id, action_id, target_id, platform, subreddit, place_id, \
              title, body, relay_source_id, status) \
         VALUES ($1,$2,$3,'reddit',$4,$5,'t','b',$6,'pending')",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(target_id)
    .bind(subreddit)
    .bind(place_id)
    .bind(source_id)
    .execute(pool)
    .await
    .context("insert batch delivery")?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_batch_posts_its_largest_surviving_community_first() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let source_id = content_source(&pool, ws).await?;

    sqlx::query(
        "INSERT INTO community_relay_batches \
             (workspace_id, source_id, status, approved_at, approved_by, interval_seconds, observe_until) \
         VALUES ($1,$2,'approved',now(),'operator:community_relay',3600, now() + INTERVAL '7 days')",
    )
    .bind(ws.into_uuid())
    .bind(source_id)
    .execute(&pool)
    .await
    .context("insert approved batch")?;

    // Seeded in this order on purpose: tinysub is the older draft, bigsub the
    // newer one — arrival order would post tinysub first.
    batch_delivery(&pool, ws, source_id, "tinysub", 100).await?;
    batch_delivery(&pool, ws, source_id, "bigsub", 4_000_000).await?;

    let executor = CommunityExecutorWorker::new(
        pool.clone(),
        ws,
        Duration::from_secs(30),
        true,
        None,
        "http://agents.invalid".to_owned(),
        None,
        None,
    )
    .context("build executor")?;
    let claimed = executor.claim_pending_actions().await?;
    assert_eq!(claimed.len(), 1, "one delivery per batch per sweep");

    let claimed_sub: String = sqlx::query_scalar(
        "SELECT subreddit FROM community_posts \
         WHERE workspace_id=$1 AND status='posting'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await
    .context("read claimed delivery")?;
    assert_eq!(claimed_sub, "bigsub", "the larger community leads the drip");
    Ok(())
}
