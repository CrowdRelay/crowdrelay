//! The lane ledger's aggregation against a real schema: four tables, one
//! vocabulary; lanes keyed by platform; the window is when the band *asked*;
//! the oldest unfinished request is reported; another tenant's lanes are not
//! this tenant's.

use crate::common;

use crowdrelay_domain::lane_ledger::{LaneScope, Verdict, verdict};
use crowdrelay_infra::lane_ledger::{autopost_settings, lane_rows};
use uuid::Uuid;

async fn workspace(pool: &sqlx::PgPool, label: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    let ws = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1,$2,$3)")
        .bind(ws)
        .bind(format!("{label}-{}", ws.simple()))
        .bind(label)
        .execute(pool)
        .await?;
    Ok(ws)
}

/// Every post hangs off an autopilot action, which hangs off a decision.
async fn action(pool: &sqlx::PgPool, ws: Uuid) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_decisions
             (id, workspace_id, decision_key, context, subject_kind, subject_id,
              decision_kind, confidence_basis_points, disposition, reason,
              input_snapshot, policy_snapshot, recommendation, trace_id)
         VALUES ($1,$2,$3,'growth_metrics','target_community',$4,'auto_execute',9000,
                 'auto_execute','test','{}'::jsonb,'{}'::jsonb,'{}'::jsonb,gen_random_uuid())",
    )
    .bind(decision)
    .bind(ws)
    .bind(format!("k-{decision}"))
    .bind(Uuid::now_v7())
    .execute(pool)
    .await?;
    let action = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_actions
             (id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
              idempotency_key, payload, status, action_class, trace_id, finished_at)
         VALUES ($1,$2,$3,'growth_metrics','agent.content.request','target_community',$4,$5,
                 '{}'::jsonb,'succeeded','third_party',gen_random_uuid(), now())",
    )
    .bind(action)
    .bind(ws)
    .bind(decision)
    .bind(Uuid::now_v7())
    .bind(format!("i-{action}"))
    .execute(pool)
    .await?;
    Ok(action)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn lanes_are_platforms_across_four_tables_and_the_verdict_names_where_each_stops()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool, "lanes").await?;
    let other = workspace(&pool, "lanes-other").await?;

    // Reddit: fifteen drafts held for a person, four failed, none delivered —
    // the 2026-10-02 production shape. The oldest held one is 50 hours old.
    for i in 0..15 {
        sqlx::query(
            "INSERT INTO community_posts (workspace_id, action_id, platform, subreddit, title, body, status, created_at)
             VALUES ($1,$2,'reddit',$3,'t','b','awaiting_manual_post', now() - make_interval(hours => $4))",
        )
        .bind(ws)
        .bind(action(&pool, ws).await?)
        .bind(format!("sub{i}"))
        .bind(if i == 0 { 50 } else { 5 })
        .execute(&pool)
        .await?;
    }
    for i in 0..4 {
        sqlx::query(
            "INSERT INTO community_posts (workspace_id, action_id, platform, subreddit, title, body, status)
             VALUES ($1,$2,'reddit',$3,'t','b','failed')",
        )
        .bind(ws)
        .bind(action(&pool, ws).await?)
        .bind(format!("failed{i}"))
        .execute(&pool)
        .await?;
    }
    // A forum draft shares the table but is its own lane.
    sqlx::query(
        "INSERT INTO community_posts (workspace_id, action_id, platform, subreddit, title, body, status)
         VALUES ($1,$2,'forum','The Black Vault','t','b','awaiting_manual_post')",
    )
    .bind(ws)
    .bind(action(&pool, ws).await?)
    .execute(&pool)
    .await?;
    // A joined/community Telegram can be blocked while the band's own
    // Telegram channel is delivering. Platform name alone must never merge
    // those two authority surfaces.
    sqlx::query(
        "INSERT INTO community_posts
             (workspace_id, action_id, platform, subreddit, title, body, status)
         VALUES ($1,$2,'telegram','metal-room','t','b','awaiting_manual_post')",
    )
    .bind(ws)
    .bind(action(&pool, ws).await?)
    .execute(&pool)
    .await?;
    // Owned Telegram delivers; Instagram delivers with one held behind it.
    for status in ["posted", "posted"] {
        sqlx::query("INSERT INTO telegram_posts (workspace_id, action_id, channel, status) VALUES ($1,$2,'@virya',$3)")
            .bind(ws)
            .bind(action(&pool, ws).await?)
            .bind(status)
            .execute(&pool)
            .await?;
    }
    for status in ["posted", "awaiting_manual_post"] {
        sqlx::query("INSERT INTO social_posts (workspace_id, action_id, platform, content, status) VALUES ($1,$2,'instagram','{}'::jsonb,$3)")
            .bind(ws)
            .bind(action(&pool, ws).await?)
            .bind(status)
            .execute(&pool)
            .await?;
    }
    // Discord channel: two queued. A post outside the window does not count.
    for _ in 0..2 {
        sqlx::query("INSERT INTO discord_posts (workspace_id, action_id, channel_id, status) VALUES ($1,$2,'123','pending')")
            .bind(ws)
            .bind(action(&pool, ws).await?)
            .execute(&pool)
            .await?;
    }
    sqlx::query(
        "INSERT INTO telegram_posts (workspace_id, action_id, channel, status, created_at)
         VALUES ($1,$2,'@virya','failed', now() - interval '40 days')",
    )
    .bind(ws)
    .bind(action(&pool, ws).await?)
    .execute(&pool)
    .await?;
    // Another tenant's delivered post is not ours.
    sqlx::query("INSERT INTO telegram_posts (workspace_id, action_id, channel, status) VALUES ($1,$2,'@x','posted')")
        .bind(other)
        .bind(action(&pool, other).await?)
        .execute(&pool)
        .await?;

    let lanes = lane_rows(&pool, ws, 14).await?;
    let find = |scope: LaneScope, name: &str| {
        lanes
            .iter()
            .find(|lane| lane.scope == scope && lane.lane == name)
            .unwrap_or_else(|| panic!("no {scope:?}/{name} lane: {lanes:?}"))
    };
    let reddit = find(LaneScope::Community, "reddit");
    assert_eq!(
        (reddit.counts.held_for_person, reddit.counts.failed),
        (15, 4)
    );
    assert_eq!(verdict(&reddit.counts), Verdict::HeldForPerson);
    assert_eq!(reddit.oldest_unfinished_hours, Some(50));
    assert_eq!(
        verdict(&find(LaneScope::Community, "forum").counts),
        Verdict::HeldForPerson
    );
    let telegram = find(LaneScope::Owned, "telegram");
    assert_eq!(telegram.counts.delivered, 2);
    assert_eq!(
        telegram.counts.failed, 0,
        "the 40-day-old failure is outside the window"
    );
    assert_eq!(verdict(&telegram.counts), Verdict::Delivering);
    assert_eq!(
        verdict(&find(LaneScope::Owned, "instagram").counts),
        Verdict::DeliveringPartly
    );
    assert_eq!(
        verdict(&find(LaneScope::Owned, "discord_channel").counts),
        Verdict::Queued
    );
    assert!(lanes.iter().all(|lane| lane.unknown_statuses == 0));
    assert_eq!(
        verdict(&find(LaneScope::Community, "telegram").counts),
        Verdict::HeldForPerson,
        "community Telegram hold must not poison owned Telegram"
    );
    assert_eq!(
        lanes.len(),
        6,
        "community reddit/forum/telegram plus owned telegram/instagram/discord_channel"
    );

    // A wider window sees the old failure; another tenant sees only its own lane.
    let wide = lane_rows(&pool, ws, 60).await?;
    assert_eq!(
        wide.iter()
            .find(|l| l.scope == LaneScope::Owned && l.lane == "telegram")
            .map(|l| l.counts.failed),
        Some(1)
    );
    let theirs = lane_rows(&pool, other, 14).await?;
    assert_eq!(theirs.len(), 1);
    assert_eq!(theirs[0].scope, LaneScope::Owned);
    assert_eq!(theirs[0].counts.delivered, 1);
    // A tenant that asked its lanes nothing has no lanes: quiet, not healthy.
    let empty = workspace(&pool, "lanes-empty").await?;
    assert!(lane_rows(&pool, empty, 14).await?.is_empty());

    // Settings are reported as stored; absent is None, not "off".
    let none = autopost_settings(&pool, ws).await?;
    assert_eq!(
        (none.social_auto_post, none.social_autopost_platforms),
        (None, None)
    );
    sqlx::query("INSERT INTO tenant_settings (workspace_id, key, value) VALUES ($1,'social_autopost_platforms','telegram')")
        .bind(ws)
        .execute(&pool)
        .await?;
    assert_eq!(
        autopost_settings(&pool, ws)
            .await?
            .social_autopost_platforms
            .as_deref(),
        Some("telegram")
    );
    Ok(())
}
