//! Relay pushes are paced, end to end through a real cycle.
//!
//! What only a real cycle proves: with three fresh band posts, one of them
//! the same words cross-posted to another platform, one cycle raises one
//! push — the newest post — and not three; the next cycle inside the gap
//! raises none; a cycle past the gap raises the other post; and the
//! cross-post is never pushed at all. On 2026-09-26 four relays reached the
//! same two fans in one tenth of a second, one of them twice.

use std::time::Duration;

use crate::common;
use crowdrelay_application::autopilot::EvaluateAutopilot;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

async fn relay_titles(
    pool: &PgPool,
    workspace_id: WorkspaceId,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar(
        "SELECT payload->>'title' FROM autopilot_actions
         WHERE workspace_id = $1 AND action_kind = 'signal.push.request'
           AND idempotency_key LIKE 'action:relay:%:signal_push'
         ORDER BY created_at, id",
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await?)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn one_relay_push_at_a_time_and_never_the_same_words_twice()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let repository = PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: database_url,
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(5),
            lock_timeout: Duration::from_secs(1),
        },
    );
    let ws = WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Relay pacing')")
        .bind(ws.into_uuid())
        .bind(format!("relay-pacing-{}", ws.into_uuid().simple()))
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO growth_envelope (workspace_id, agent_enabled, dry_run) VALUES ($1, true, false)
         ON CONFLICT (workspace_id) DO UPDATE SET agent_enabled = true, dry_run = false",
    )
    .bind(ws.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO autopilot_policies
         (workspace_id, context, enabled, autonomy_level, max_actions_24h)
         VALUES ($1, 'content_supply', true, 'bounded_auto', 30)
         ON CONFLICT (workspace_id, context) DO UPDATE
         SET enabled = true, autonomy_level = 'bounded_auto', max_actions_24h = 30",
    )
    .bind(ws.into_uuid())
    .execute(&pool)
    .await?;

    let now = OffsetDateTime::now_utc();
    // (hours ago, platform, caption, permalink). The Instagram post an hour
    // ago is the newest; the Facebook one at three hours carries the same
    // words.
    let posts = [
        (
            1,
            "instagram",
            "Terapia grupowa, spowiedź szaleńca, mental metal.",
            "https://www.instagram.com/p/Dd1/",
        ),
        (
            2,
            "facebook",
            "Nowy klip już jest.",
            "https://www.facebook.com/1069/posts/2",
        ),
        (
            3,
            "facebook",
            "Terapia grupowa, spowiedź szaleńca, mental metal.",
            "https://www.facebook.com/1069/posts/1",
        ),
    ];
    for (hours, platform, caption, url) in posts {
        sqlx::query(
            "INSERT INTO content_sources (
                 id, workspace_id, source_kind, source_key, title, occurred_at, expires_at, metadata
             ) VALUES ($1,$2,'social_post',$3,$4,$5,$6,$7)",
        )
        .bind(Uuid::now_v7())
        .bind(ws.into_uuid())
        .bind(format!("{platform}:{url}"))
        .bind(caption)
        .bind(now - time::Duration::hours(hours))
        .bind(now + time::Duration::days(30))
        .bind(serde_json::json!({ "platform": platform, "url": url, "body": caption }))
        .execute(&pool)
        .await?;
    }

    let cycle = |at: OffsetDateTime| {
        let repository = &repository;
        async move { EvaluateAutopilot::new(repository, ws).execute(at).await }
    };

    let report = cycle(now).await?;
    assert_eq!(
        relay_titles(&pool, ws).await?,
        ["Terapia grupowa, spowiedź szaleńca, mental metal."],
        "one push, the newest post (report: {report:?})"
    );

    cycle(now + time::Duration::minutes(5)).await?;
    assert_eq!(relay_titles(&pool, ws).await?.len(), 1, "inside the gap");

    cycle(now + time::Duration::hours(13)).await?;
    cycle(now + time::Duration::hours(26)).await?;
    assert_eq!(
        relay_titles(&pool, ws).await?,
        [
            "Terapia grupowa, spowiedź szaleńca, mental metal.",
            "Nowy klip już jest."
        ],
        "the other post past the gap, and the cross-post never"
    );
    Ok(())
}
