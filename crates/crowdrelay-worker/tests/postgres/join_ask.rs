//! Does a decided join-ask become a post artifact with a tracked link?
//!
//! The join-ask executor path is the social executor's second insert (§5):
//! a `social.join_ask.publish` action is raised by the evaluator itself, so
//! there is no `agent_service_tasks` row to join through. This proves the
//! claim shape end to end — the `social_posts` row, the minted `smart_links`
//! row it binds, and the channel source the click measurement reads back.

use crate::common;

use anyhow::{Context, Result, ensure};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_worker::social_post_executor::SocialPostExecutorWorker;
use serde_json::json;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

async fn workspace(pool: &PgPool) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("joinask-{}", id.simple()))
        .bind("Join Ask Test")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(WorkspaceId::from_uuid(id))
}

/// `agent_service_tasks` belongs to the agents service — no CrowdRelay
/// migration creates it — but the executor's *other* insert joins through
/// it, so the suite database needs the columns that statement names even
/// when the test itself files no draft. Same shim as
/// `publication_artifact.rs`.
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
    .await
    .context("create the foreign agent task table")?;
    Ok(())
}

/// The shape the evaluator persists: a succeeded action whose flat payload
/// carries the platform, the chosen variant index, the verbatim text and
/// the member-site CTA. No task row — the brain wrote this one itself.
async fn seed_join_ask_action(pool: &PgPool, workspace_id: WorkspaceId) -> Result<Uuid> {
    seed_join_ask_action_for(pool, workspace_id, "facebook", None).await
}

/// Same persisted shape, for one platform and an optional fixed image — a
/// join-ask carrying `join_ask_image_url` writes it onto the payload.
async fn seed_join_ask_action_for(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    platform: &str,
    image_url: Option<&str>,
) -> Result<Uuid> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, trace_id)
           VALUES ($1,$2,$3,'promotion_budget','workspace',$4,
                   'publish_join_ask',7000,'require_approval','test',
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,$5)"#,
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("joinask-{decision_id}"))
    .bind(workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert decision")?;

    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, finished_at)
           VALUES ($1,$2,$3,'promotion_budget','social.join_ask.publish','workspace',
                   $4,$5,$6,'succeeded',now())"#,
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("join_ask:{platform}:2026-W39"))
    .bind(json!({
        "platform": platform,
        "variant_index": 0,
        "text": "Join us on Signal.",
        "cta_url": format!("https://virya.music/signal?utm_source={platform}&utm_medium=join_ask&utm_campaign=join_ask_w39"),
        "image_url": image_url,
    }))
    .execute(pool)
    .await
    .context("insert action")?;
    Ok(action_id)
}

fn executor(pool: &PgPool, workspace_id: WorkspaceId) -> SocialPostExecutorWorker {
    // manual_mode = true: draft only, no network — the artifact waits for a
    // person the same way an agent draft does.
    SocialPostExecutorWorker::new(
        pool.clone(),
        workspace_id,
        true,
        None,
        "https://virya.music".to_owned(),
        crowdrelay_infra::sensitive_response::SensitiveResponseKey::derive_from_secret(
            b"test-encryption-key",
        ),
        false,
    )
    .expect("build executor")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_join_ask_becomes_a_tracked_artifact_awaiting_a_person() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    let ws = workspace(&database).await?;
    create_foreign_task_table(&database).await?;
    let action_id = seed_join_ask_action(&database, ws).await?;

    executor(&database, ws).run_once().await?;

    let row = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            String,
            Option<String>,
            Option<Uuid>,
            serde_json::Value,
        ),
    >(
        "SELECT id, platform, status, smart_link, smart_link_id, content
         FROM social_posts WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(ws.into_uuid())
    .bind(action_id)
    .fetch_one(&database)
    .await
    .context("read the join-ask social_posts row")?;
    let (social_post_id, platform, status, smart_link, smart_link_id, content) = row;
    ensure!(
        platform == "facebook",
        "platform must survive, got {platform}"
    );
    ensure!(
        status == "awaiting_manual_post",
        "a manual-mode join-ask must wait for a person, got {status}"
    );
    ensure!(
        content["join_ask"] == json!(true),
        "the post must carry the join-ask marker, got {content}"
    );
    let expected_slug = format!("social-{}", action_id.simple());
    ensure!(
        smart_link.as_deref() == Some(format!("/l/{expected_slug}").as_str()),
        "the post must carry its tracked link, got {smart_link:?}"
    );
    let link_id = smart_link_id.context("a tracked post must bind a smart_link_id")?;

    let link = sqlx::query_as::<_, (String, String, Option<String>, Option<Uuid>)>(
        "SELECT slug, destination_url, channel_source, action_id
         FROM smart_links WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws.into_uuid())
    .bind(link_id)
    .fetch_one(&database)
    .await
    .context("read the minted smart_links row")?;
    ensure!(link.0 == expected_slug, "slug mismatch: {}", link.0);
    ensure!(
        link.1
            == "https://virya.music/signal?utm_source=facebook&utm_medium=join_ask&utm_campaign=join_ask_w39",
        "the link must land on the member-site CTA, got {}",
        link.1
    );
    ensure!(
        link.2.as_deref() == Some("facebook"),
        "channel_source must name the platform, got {:?}",
        link.2
    );
    ensure!(
        link.3 == Some(action_id),
        "the acquisition link must be owned by the action that published it, got {:?}",
        link.3
    );

    // Internal dispatch and a filed draft are not publication evidence.
    let premature_outcomes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM autopilot_outcomes
         WHERE workspace_id=$1 AND action_id=$2 AND metric_key='join_ask_published'",
    )
    .bind(ws.into_uuid())
    .bind(action_id)
    .fetch_one(&database)
    .await?;
    ensure!(
        premature_outcomes == 0,
        "a draft must not claim publication, got {premature_outcomes} outcomes"
    );
    let premature_measurements: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM autopilot_measurements
         WHERE workspace_id=$1 AND action_id=$2
           AND measurement_kind IN ('content_link_clicks_7d','content_fan_acquisition_7d')",
    )
    .bind(ws.into_uuid())
    .bind(action_id)
    .fetch_one(&database)
    .await?;
    ensure!(
        premature_measurements == 0,
        "measurement windows must wait for provider publication, got {premature_measurements}"
    );

    let before = crowdrelay_infra::organic_funnel::read(
        &database,
        ws.into_uuid(),
        Some(action_id),
        None,
        7,
        10,
        OffsetDateTime::now_utc(),
    )
    .await?;
    ensure!(before.len() == 1, "expected one funnel link, got {}", before.len());
    ensure!(
        before[0].published_at.is_none() && before[0].diagnosis == "publication_unverified",
        "draft must remain publication_unverified, got {:?} / {}",
        before[0].published_at,
        before[0].diagnosis
    );

    // Register a real provider receipt. This is the first moment publication
    // evidence and the click/fan windows are allowed to exist.
    crowdrelay_infra::fanbase::register_manual_social_post(
        &database,
        ws.into_uuid(),
        social_post_id,
        "https://www.facebook.com/joinask-provider-post",
        Some("joinask-provider-post"),
    )
    .await?;

    let confirmed_outcomes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM autopilot_outcomes
         WHERE workspace_id=$1 AND action_id=$2 AND metric_key='join_ask_published'",
    )
    .bind(ws.into_uuid())
    .bind(action_id)
    .fetch_one(&database)
    .await?;
    ensure!(
        confirmed_outcomes == 1,
        "provider receipt must create exactly one publication outcome, got {confirmed_outcomes}"
    );
    let confirmed_measurements: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM autopilot_measurements
         WHERE workspace_id=$1 AND action_id=$2
           AND measurement_kind IN ('content_link_clicks_7d','content_fan_acquisition_7d')",
    )
    .bind(ws.into_uuid())
    .bind(action_id)
    .fetch_one(&database)
    .await?;
    ensure!(
        confirmed_measurements == 2,
        "provider receipt must open exactly the click and fan windows, got {confirmed_measurements}"
    );

    let receipt = sqlx::query_as::<_, (Option<String>, Option<String>, Option<time::OffsetDateTime>)>(
        "SELECT platform_post_id, platform_post_url, posted_at
         FROM social_posts WHERE workspace_id=$1 AND id=$2",
    )
    .bind(ws.into_uuid())
    .bind(social_post_id)
    .fetch_one(&database)
    .await?;
    ensure!(
        receipt.0.as_deref() == Some("joinask-provider-post")
            && receipt.1.as_deref() == Some("https://www.facebook.com/joinask-provider-post")
            && receipt.2.is_some(),
        "durable provider receipt missing: {receipt:?}"
    );

    let after = crowdrelay_infra::organic_funnel::read(
        &database,
        ws.into_uuid(),
        Some(action_id),
        None,
        7,
        10,
        OffsetDateTime::now_utc(),
    )
    .await?;
    ensure!(after.len() == 1, "expected one funnel row after receipt");
    ensure!(
        after[0].published_at.is_some() && after[0].diagnosis == "awaiting_traffic_window",
        "provider receipt must open the traffic window, got {:?} / {}",
        after[0].published_at,
        after[0].diagnosis
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_telegram_join_ask_files_its_image_and_waits_for_a_person() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    let ws = workspace(&database).await?;
    create_foreign_task_table(&database).await?;
    let action_id = seed_join_ask_action_for(
        &database,
        ws,
        "telegram",
        Some("https://signal-api.virya.music/v1/public/media/00000000-0000-7000-8000-000000000001"),
    )
    .await?;

    // manual_mode + telegram_auto_post=false: the row files, then waits —
    // the publish path itself needs a live Bot API, which is not this test.
    executor(&database, ws).run_once().await?;

    let (platform, status, image_url, content) =
        sqlx::query_as::<_, (String, String, Option<String>, serde_json::Value)>(
            "SELECT platform, status, image_url, content
         FROM social_posts WHERE workspace_id = $1 AND action_id = $2",
        )
        .bind(ws.into_uuid())
        .bind(action_id)
        .fetch_one(&database)
        .await
        .context("read the telegram join-ask row")?;
    ensure!(
        platform == "telegram",
        "platform must survive, got {platform}"
    );
    ensure!(
        status == "awaiting_manual_post",
        "a telegram ask with auto-post off must wait for a person, got {status}"
    );
    ensure!(
        image_url.as_deref()
            == Some(
                "https://signal-api.virya.music/v1/public/media/00000000-0000-7000-8000-000000000001"
            ),
        "the fixed image must land on the row, got {image_url:?}"
    );
    ensure!(
        content["join_ask"] == json!(true),
        "the post must carry the join-ask marker, got {content}"
    );
    Ok(())
}
