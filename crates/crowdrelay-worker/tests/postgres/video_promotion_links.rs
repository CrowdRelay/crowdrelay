//! Community links share the release campaign and never wrap another link.
use crate::{
    common,
    community_relay_batch::{
        action_payload, action_rows, community_target, content_source, engage_outcome, worker,
        workspace,
    },
};
use anyhow::{Result, ensure};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database"]
async fn promotion_links_share_the_release_campaign_and_repair_source_owned_legacy_links()
-> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool).await?;
    let source = content_source(&pool, ws).await?;
    let canonical = "https://www.youtube.com/watch?v=promotion-test";
    sqlx::query("UPDATE content_sources SET metadata=jsonb_set(metadata,'{url}',$3) WHERE workspace_id=$1 AND id=$2")
        .bind(ws.into_uuid()).bind(source).bind(json!(canonical)).execute(&pool).await?;
    let plan: Uuid=sqlx::query_scalar("INSERT INTO release_plans(workspace_id,source_key,title,release_at,listen_url) VALUES($1,$2,'Release',now(),$3) RETURNING id")
        .bind(ws.into_uuid()).bind(Uuid::now_v7().to_string()).bind(canonical).fetch_one(&pool).await?;
    let campaign: Uuid=sqlx::query_scalar("INSERT INTO campaigns(workspace_id,name,release_plan_id) VALUES($1,'Release acquisition',$2) RETURNING id")
        .bind(ws.into_uuid()).bind(plan).fetch_one(&pool).await?;
    let first = community_target(&pool, ws, "firstcommunity").await?;
    let outcome = engage_outcome(&pool, ws, first, source, "firstcommunity").await?;
    sqlx::query("UPDATE agent_outcomes SET payload=jsonb_set(payload,'{item,smart_link}',$3) WHERE workspace_id=$1 AND id=$2")
        .bind(ws.into_uuid()).bind(outcome).bind(json!("https://virya.music/l/existing-release")).execute(&pool).await?;
    worker(&pool, ws).run_once().await?;
    let actions = action_rows(&pool, ws).await?;
    ensure!(actions.len() == 1, "the video draft maps");
    let payload = action_payload(&pool, actions[0].0).await?;
    let slug = payload["smart_link"]
        .as_str()
        .expect("tracked")
        .trim_start_matches("/l/");
    let (destination, linked): (String, Option<Uuid>) = sqlx::query_as(
        "SELECT destination_url,campaign_id FROM smart_links WHERE workspace_id=$1 AND slug=$2",
    )
    .bind(ws.into_uuid())
    .bind(slug)
    .fetch_one(&pool)
    .await?;
    ensure!(
        destination == canonical && linked == Some(campaign),
        "direct canonical URL and existing campaign"
    );
    // An existing public slug with missing campaign ownership is repaired,
    // not replaced; no historical event is rewritten as a new fan.
    sqlx::query("UPDATE smart_links SET campaign_id=NULL,destination_url='https://virya.music/l/existing-release' WHERE workspace_id=$1 AND slug=$2")
        .bind(ws.into_uuid()).bind(slug).execute(&pool).await?;
    let second = community_target(&pool, ws, "secondcommunity").await?;
    engage_outcome(&pool, ws, second, source, "secondcommunity").await?;
    worker(&pool, ws).run_once().await?;
    let links: Vec<(String,Option<Uuid>)>=sqlx::query_as("SELECT destination_url,campaign_id FROM smart_links WHERE workspace_id=$1 AND slug LIKE 'agent-%'")
        .bind(ws.into_uuid()).fetch_all(&pool).await?;
    ensure!(links.len() == 2);
    ensure!(
        links
            .iter()
            .all(|(url, id)| url == canonical && *id == Some(campaign)),
        "every community shares source identity"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database"]
async fn a_forum_outcome_keeps_its_platform_and_source_policy_can_refuse_it() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool).await?;
    let source = content_source(&pool, ws).await?;
    let target = community_target(&pool, ws, "joinedforum").await?;
    sqlx::query("UPDATE agent_outreach_targets SET platform='forum',subreddit=NULL,community_url='https://forum.example/music' WHERE workspace_id=$1 AND id=$2")
        .bind(ws.into_uuid()).bind(target).execute(&pool).await?;
    let first = engage_outcome(&pool, ws, target, source, "joinedforum").await?;
    sqlx::query("UPDATE agent_outcomes SET payload=jsonb_set(payload,'{item,platform}','\"forum\"') WHERE workspace_id=$1 AND id=$2")
        .bind(ws.into_uuid()).bind(first).execute(&pool).await?;
    worker(&pool, ws).run_once().await?;
    let actions = action_rows(&pool, ws).await?;
    ensure!(actions.len() == 1);
    ensure!(action_payload(&pool, actions[0].0).await?["platform"] == "forum");
    sqlx::query("UPDATE content_sources SET metadata=jsonb_set(metadata,'{promotion_excluded_platforms}',$3) WHERE workspace_id=$1 AND id=$2")
        .bind(ws.into_uuid()).bind(source).bind(json!(["forum"])).execute(&pool).await?;
    let refused = engage_outcome(&pool, ws, target, source, "joinedforum").await?;
    worker(&pool, ws).run_once().await?;
    let status: String =
        sqlx::query_scalar("SELECT status FROM agent_outcomes WHERE workspace_id=$1 AND id=$2")
            .bind(ws.into_uuid())
            .bind(refused)
            .fetch_one(&pool)
            .await?;
    ensure!(status == "rejected");
    ensure!(
        action_rows(&pool, ws).await?.len() == 1,
        "excluded draft creates no new post"
    );
    Ok(())
}
