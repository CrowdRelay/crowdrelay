//! The subreddit cooldown is per-community, not flat: a place whose
//! `discovery_place_rules.cooldown_days` declares 14 days must not see a
//! second band post on day 8, where the flat seven-day floor would let it
//! through. A place with no rules row keeps the floor. Runs under
//! `just test-postgres` against `CROWDRELAY_TEST_DATABASE_URL`.

use crate::common;

use std::time::Duration;

use anyhow::{Context, Result};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_worker::community_executor::CommunityExecutorWorker;
use sqlx::PgPool;
use uuid::Uuid;

use super::community_relay_batch::workspace;

async fn place_with_rules(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    name: &str,
    cooldown_days: Option<i16>,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO discovery_places (id, workspace_id, place_kind, platform, name, url) \
         VALUES ($1,$2,'subreddit','reddit',$3,$4)",
    )
    .bind(id)
    .bind(workspace_id.into_uuid())
    .bind(name)
    .bind(format!("https://www.reddit.com/r/{name}"))
    .execute(pool)
    .await
    .context("insert discovery place")?;
    if let Some(days) = cooldown_days {
        sqlx::query("INSERT INTO discovery_place_rules (place_id, cooldown_days) VALUES ($1,$2)")
            .bind(id)
            .bind(days)
            .execute(pool)
            .await
            .context("insert place rules")?;
    }
    Ok(id)
}

async fn engage_action(pool: &PgPool, workspace_id: WorkspaceId) -> Result<Uuid> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id
        ) VALUES ($1,$2,$3,'growth_intelligence','workspace',$4,
                  'community.engage',9000,'auto_execute','post to a community',
                  '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)
        "#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("engage-{decision_id}"))
    .bind(workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert engage decision")?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, finished_at, trace_id
        ) VALUES ($1,$2,$3,'growth_intelligence','community.engage.request',
                  'workspace',$4,$5,'{}'::jsonb,'succeeded',now(),$6)
        "#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("action-{action_id}"))
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert engage action")?;
    Ok(action_id)
}

/// A `posted` row as the claim gate's cooldown check sees it — the place the
/// post went to is what carries the rules.
async fn posted_row(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    subreddit: &str,
    place_id: Uuid,
    days_ago: i32,
) -> Result<()> {
    let action_id = engage_action(pool, workspace_id).await?;
    sqlx::query(
        "INSERT INTO community_posts \
             (workspace_id, action_id, platform, subreddit, place_id, title, body, status, posted_at) \
         VALUES ($1,$2,'reddit',$3,$4,'old post','body','posted', now() - make_interval(days => $5))",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(subreddit)
    .bind(place_id)
    .bind(days_ago)
    .execute(pool)
    .await
    .context("insert posted row")?;
    Ok(())
}

/// A pending delivery the claim sweep would take if no gate stopped it.
async fn pending_post(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    subreddit: &str,
    target_id: Uuid,
) -> Result<()> {
    let action_id = engage_action(pool, workspace_id).await?;
    sqlx::query(
        "INSERT INTO community_posts \
             (workspace_id, action_id, platform, subreddit, target_id, title, body, status) \
         VALUES ($1,$2,'reddit',$3,$4,'queued','body','pending')",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(subreddit)
    .bind(target_id)
    .execute(pool)
    .await
    .context("insert pending post")?;
    Ok(())
}

async fn post_status(pool: &PgPool, workspace_id: WorkspaceId, subreddit: &str) -> Result<String> {
    sqlx::query_scalar(
        "SELECT status FROM community_posts \
         WHERE workspace_id=$1 AND subreddit=$2 AND status <> 'posted' \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(workspace_id.into_uuid())
    .bind(subreddit)
    .fetch_one(pool)
    .await
    .context("read pending post status")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_communitys_own_cooldown_extends_the_flat_floor() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;

    // metalcore declares a 14-day cooldown in its rules; testsub declares
    // nothing. Both saw a post eight days ago — past the flat seven-day
    // floor, inside the rules cooldown for the first only.
    let gated_place = place_with_rules(&pool, ws, "metalcore", Some(14)).await?;
    let open_place = place_with_rules(&pool, ws, "testsub", None).await?;
    posted_row(&pool, ws, "metalcore", gated_place, 8).await?;
    posted_row(&pool, ws, "testsub", open_place, 8).await?;

    for subreddit in ["metalcore", "testsub"] {
        let target = super::community_relay_batch::community_target(&pool, ws, subreddit).await?;
        pending_post(&pool, ws, subreddit, target).await?;
    }

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
    executor.claim_pending_actions().await?;

    assert_eq!(
        post_status(&pool, ws, "metalcore").await?,
        "pending",
        "a 14-day community must not see a second post on day 8"
    );
    assert_eq!(
        post_status(&pool, ws, "testsub").await?,
        "posting",
        "no rules row means the flat floor governs — day 8 is clear"
    );
    Ok(())
}
