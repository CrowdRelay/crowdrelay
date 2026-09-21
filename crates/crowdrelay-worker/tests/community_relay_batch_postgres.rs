//! The community relay batch against a real Postgres.
//!
//! Fifty drafts carrying one synced post are one question — "does this
//! content go to the communities that will take it" — not fifty cards and
//! fifty emails. These tests drive a real ingestion cycle and assert the
//! batch is that question's durable answer: the first draft creates it and
//! notifies once, later drafts join its parked set silently, approval
//! releases the whole spread at the card's cadence, revoke cancels the drip,
//! and a batch that was already answered rejects new drafts instead of
//! re-asking.

mod common;

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use crowdrelay_application::IdempotencyKey;
use crowdrelay_application::autopilot::AutopilotControlRepository;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use crowdrelay_worker::agent_outcomes::AgentOutcomeWorker;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

async fn workspace(pool: &PgPool) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("relay-batch-{}", id.simple()))
        .bind("Relay Batch Test")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(WorkspaceId::from_uuid(id))
}

fn worker(pool: &PgPool, workspace_id: WorkspaceId) -> AgentOutcomeWorker {
    AgentOutcomeWorker::new(
        pool.clone(),
        workspace_id,
        Duration::from_secs(60),
        Duration::from_secs(30),
        "https://virya.music".to_owned(),
        crowdrelay_worker::auto_post_platforms::AutoPostPlatforms::default(),
    )
}

fn repository(pool: &PgPool) -> PostgresAutopilotRepository {
    PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: "postgres://unused-in-test".to_owned(),
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        },
    )
}

/// The content the drafts carry — a synced post's row, with the media the
/// approval card shows and the posts attach. The batch keys on this row's id.
async fn content_source(pool: &PgPool, workspace_id: WorkspaceId) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_content_sources (
             id, workspace_id, source_kind, source_key, title, occurred_at,
             expires_at, metadata
         ) VALUES ($1,$2,'video',$3,$4, now() - INTERVAL '2 hours',
                   now() + INTERVAL '30 days', $5)",
    )
    .bind(id)
    .bind(workspace_id.into_uuid())
    .bind(format!("video-{}", id.simple()))
    .bind("The band's new video")
    .bind(json!({
        "media_url": "https://cdn.example.com/video.mp4",
        "thumbnail_url": "https://cdn.example.com/thumb.jpg",
        "media_type": "VIDEO",
        "media_id": "media-1",
        "url": "https://reddit.com/r/band/comments/abc",
    }))
    .execute(pool)
    .await
    .context("insert content source")?;
    Ok(id)
}

/// A screened-and-admitted community — the bar the ingest gate checks.
async fn community_target(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    subreddit: &str,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO agent_outreach_targets (
             id, workspace_id, target_kind, display_name, why_fit, status,
             screening_verdict, subreddit
         ) VALUES ($1,$2,'community',$3,'active metal community','promoted',
                   'admitted',$4)",
    )
    .bind(id)
    .bind(workspace_id.into_uuid())
    .bind(format!("r/{subreddit}"))
    .bind(subreddit)
    .execute(pool)
    .await
    .context("insert community target")?;
    Ok(id)
}

/// A community-engager draft as the agents service emits it: one outcome per
/// community the post was drafted for, all naming the same source.
async fn engage_outcome(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    target_id: Uuid,
    source_id: Uuid,
    subreddit: &str,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO agent_outcomes (
            id, workspace_id, task_id, result_id, kind, schema_version,
            payload, confidence_basis_points, idempotency_key, status
        ) VALUES ($1,$2,$3,$4,'social_post',1,$5,7500,$6,'pending')
        "#,
    )
    .bind(id)
    .bind(workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .bind(json!({
        "item": {
            "platform": "reddit",
            "target_id": target_id.to_string(),
            "subreddit": subreddit,
            "title": "New video is out",
            "body": "The band just dropped it — what do you think?",
            "source_id": source_id.to_string(),
        },
        "rationale": "the community takes band news",
        "provenance": {
            "verification": { "status": "grounding_check_passed" },
            "context": { "any_source_failed": false, "any_source_truncated": false },
            "confidence": { "basis_points": 7500, "source": "model_self_report", "is_evidence_confidence": false },
            "model": { "actual": "test-model", "provider": "test" }
        }
    }))
    .bind(format!("relay-batch-test-{id}"))
    .execute(pool)
    .await
    .context("insert engage outcome")?;
    Ok(id)
}

async fn action_rows(
    pool: &PgPool,
    workspace_id: WorkspaceId,
) -> Result<Vec<(Uuid, String, Option<String>, Option<time::OffsetDateTime>)>> {
    Ok(
        sqlx::query_as::<_, (Uuid, String, Option<String>, Option<time::OffsetDateTime>)>(
            "SELECT id, status, approved_by, approval_expires_at \
         FROM viryaos_autopilot_actions \
         WHERE workspace_id = $1 AND action_kind = 'community.engage.request' \
         ORDER BY created_at",
        )
        .bind(workspace_id.into_uuid())
        .fetch_all(pool)
        .await?,
    )
}

async fn approval_event_count(pool: &PgPool, workspace_id: WorkspaceId) -> Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM outbox_events \
         WHERE workspace_id = $1 \
           AND event_type = 'crowdrelay.autopilot.approval_requested'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(pool)
    .await?)
}

async fn batch_row(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    source_id: Uuid,
) -> Result<Option<(String, Option<time::OffsetDateTime>)>> {
    Ok(sqlx::query_as::<_, (String, Option<time::OffsetDateTime>)>(
        "SELECT status, observe_until FROM community_relay_batches \
         WHERE workspace_id = $1 AND source_id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(source_id)
    .fetch_optional(pool)
    .await?)
}

// ── The collapse: N drafts, one batch, one notification ──────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn many_communities_one_source_is_one_approval() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let source_id = content_source(&pool, ws).await?;
    let target_a = community_target(&pool, ws, "metalpolska").await?;
    let target_b = community_target(&pool, ws, "heavymetal").await?;

    engage_outcome(&pool, ws, target_a, source_id, "metalpolska").await?;
    engage_outcome(&pool, ws, target_b, source_id, "heavymetal").await?;

    let processed = worker(&pool, ws).run_once().await?;
    if processed != 2 {
        let reasons: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT status, rejection_reason FROM agent_outcomes WHERE workspace_id = $1",
        )
        .bind(ws.into_uuid())
        .fetch_all(&pool)
        .await?;
        panic!("both drafts processed, got {processed}; outcomes: {reasons:?}");
    }

    let batch = batch_row(&pool, ws, source_id).await?;
    let (status, _) = batch.expect("one batch row for the source");
    ensure!(status == "awaiting_approval", "batch parks, got {status}");

    let actions = action_rows(&pool, ws).await?;
    ensure!(
        actions.len() == 2,
        "each community keeps its own delivery row, got {}",
        actions.len()
    );
    for (_, status, _, expires) in &actions {
        ensure!(
            status == "awaiting_approval",
            "delivery parks, got {status}"
        );
        ensure!(
            expires.is_none(),
            "a batched delivery has no per-action expiry — the batch card is the standing question"
        );
    }

    // The whole point: one notification for the spread, not one per subreddit.
    ensure!(
        approval_event_count(&pool, ws).await? == 1,
        "one approval notification per batch, not per community"
    );
    let relay_flagged = sqlx::query_scalar::<_, bool>(
        "SELECT (payload->>'relay_batch')::boolean FROM outbox_events \
         WHERE workspace_id = $1 \
           AND event_type = 'crowdrelay.autopilot.approval_requested'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(
        relay_flagged,
        "the single notification identifies as the batch"
    );
    Ok(())
}

// ── Approval releases the spread; late drafts queue under it ─────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn approving_the_batch_releases_every_delivery_and_late_drafts_queue() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let source_id = content_source(&pool, ws).await?;
    let target_a = community_target(&pool, ws, "metalpolska").await?;
    let target_b = community_target(&pool, ws, "heavymetal").await?;
    let target_c = community_target(&pool, ws, "deathmetal").await?;

    engage_outcome(&pool, ws, target_a, source_id, "metalpolska").await?;
    engage_outcome(&pool, ws, target_b, source_id, "heavymetal")
        .await
        .context("late outcome")?;
    worker(&pool, ws)
        .run_once()
        .await
        .context("second run_once")?;

    let mutation = repository(&pool)
        .approve_community_relay(
            ws,
            source_id,
            None,
            None,
            &IdempotencyKey::parse("batch-approve-1")?,
            None,
        )
        .await?;
    ensure!(
        mutation.status == "approved:2",
        "the whole parked spread moved at once, got {}",
        mutation.status
    );

    let actions = action_rows(&pool, ws).await?;
    for (_, status, approved_by, _) in &actions {
        ensure!(status == "queued", "released delivery queues, got {status}");
        ensure!(
            approved_by.as_deref() == Some("operator:community_relay"),
            "the batch's provenance, not a per-card approval"
        );
    }
    let (status, observe_until) = batch_row(&pool, ws, source_id).await?.expect("batch");
    ensure!(status == "approved", "batch approved, got {status}");
    let observe_until = observe_until.expect("approval opens the observation window");
    let window = observe_until - time::OffsetDateTime::now_utc();
    ensure!(
        window > time::Duration::days(6) && window < time::Duration::days(8),
        "observation is approval + a week, got {window}"
    );

    // The approval is a standing answer: a draft that lands after it queues
    // into the drip directly — a second parked card for it is the flood the
    // batch exists to end.
    engage_outcome(&pool, ws, target_c, source_id, "deathmetal").await?;
    worker(&pool, ws).run_once().await?;
    let actions = action_rows(&pool, ws).await?;
    ensure!(
        actions.len() == 3,
        "the late draft joined, got {}",
        actions.len()
    );
    ensure!(
        actions[2].1 == "queued",
        "a draft under an approved batch queues directly, got {}",
        actions[2].1
    );
    ensure!(
        approval_event_count(&pool, ws).await? == 1,
        "still one notification — the late draft asked nothing"
    );

    // The replay says what happened without recounting it.
    let replay = repository(&pool)
        .approve_community_relay(
            ws,
            source_id,
            None,
            None,
            &IdempotencyKey::parse("batch-approve-1")?,
            None,
        )
        .await?;
    ensure!(replay.replayed, "same key replays");
    ensure!(
        replay.status == "approved",
        "replay reports approval, not a recount"
    );
    Ok(())
}

// ── Revoke cancels the drip and closes the door ──────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn revoking_the_batch_cancels_the_drip_and_rejects_late_drafts() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let source_id = content_source(&pool, ws).await?;
    let target_a = community_target(&pool, ws, "metalpolska").await?;
    let target_b = community_target(&pool, ws, "heavymetal").await?;

    engage_outcome(&pool, ws, target_a, source_id, "metalpolska")
        .await
        .context("seed outcome")?;
    worker(&pool, ws)
        .run_once()
        .await
        .context("first run_once")?;

    repository(&pool)
        .approve_community_relay(
            ws,
            source_id,
            None,
            None,
            &IdempotencyKey::parse("batch-approve-2")?,
            None,
        )
        .await
        .context("approve")?;

    // The drip row the executor seeds for a released delivery — the thing
    // revoke must stop from ever reaching Reddit. The action stays `queued`:
    // the action ledger forbids a queued→succeeded jump (the real path goes
    // through `processing`), and what revoke must prove is that a queued
    // delivery and its seeded post both stop.
    let action_id = action_rows(&pool, ws).await?[0].0;
    sqlx::query(
        "INSERT INTO community_posts \
             (workspace_id, action_id, target_id, subreddit, title, body, relay_source_id, status) \
         VALUES ($1,$2,$3,'metalpolska','t','b',$4,'pending')",
    )
    .bind(ws.into_uuid())
    .bind(action_id)
    .bind(target_a)
    .bind(source_id)
    .execute(&pool)
    .await
    .context("seed community_post")?;

    let mutation = repository(&pool)
        .revoke_community_relay(
            ws,
            source_id,
            &IdempotencyKey::parse("batch-revoke-1")?,
            None,
        )
        .await
        .context("revoke")?;
    ensure!(
        mutation.status.starts_with("revoked:"),
        "revoke reports what it cancelled, got {}",
        mutation.status
    );

    let (_, status, _, _) = action_rows(&pool, ws).await?[0].clone();
    ensure!(
        status == "cancelled",
        "the released delivery stops, got {status}"
    );
    let post_status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM community_posts WHERE workspace_id=$1 AND relay_source_id=$2",
    )
    .bind(ws.into_uuid())
    .bind(source_id)
    .fetch_one(&pool)
    .await?;
    ensure!(
        post_status == "cancelled",
        "the drip row stops too, got {post_status}"
    );

    // A revoked batch rejects the next draft rather than parking a new ask.
    engage_outcome(&pool, ws, target_b, source_id, "heavymetal")
        .await
        .context("late outcome")?;
    worker(&pool, ws)
        .run_once()
        .await
        .context("second run_once")?;
    ensure!(
        action_rows(&pool, ws).await?.len() == 1,
        "no new delivery after revoke"
    );
    let rejected = sqlx::query_scalar::<_, Option<String>>(
        "SELECT rejection_reason FROM agent_outcomes \
         WHERE workspace_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(
        rejected
            .as_deref()
            .is_some_and(|r| r.contains("RELAY_BATCH_CLOSED")),
        "the late draft is rejected as a closed batch, got {rejected:?}"
    );
    ensure!(
        approval_event_count(&pool, ws).await? == 1,
        "a rejected draft notifies nobody"
    );
    Ok(())
}

// ── The card reads the campaign back ─────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_batch_card_reads_back_the_campaign() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let source_id = content_source(&pool, ws).await?;
    let target_a = community_target(&pool, ws, "metalpolska").await?;
    let target_b = community_target(&pool, ws, "heavymetal").await?;

    engage_outcome(&pool, ws, target_a, source_id, "metalpolska").await?;
    engage_outcome(&pool, ws, target_b, source_id, "heavymetal")
        .await
        .context("late outcome")?;
    worker(&pool, ws)
        .run_once()
        .await
        .context("second run_once")?;

    let views = repository(&pool).load_community_relays(ws).await?;
    ensure!(
        views.len() == 1,
        "one card for the spread, got {}",
        views.len()
    );
    let view = &views[0];
    ensure!(view.source_id == source_id, "the card keys on the source");
    ensure!(view.status == "awaiting_approval", "got {}", view.status);
    ensure!(
        view.interval_seconds == 3600,
        "one post an hour is the default cadence"
    );
    ensure!(
        view.image_url.as_deref() == Some("https://cdn.example.com/thumb.jpg"),
        "a VIDEO source shows its thumbnail still, got {:?}",
        view.image_url
    );
    ensure!(
        view.targets.len() == 2,
        "the card lists every community, got {}",
        view.targets.len()
    );
    ensure!(
        view.targets.iter().all(|t| t.status == "awaiting_approval"),
        "each target shows its own delivery state"
    );
    ensure!(view.sample_title.is_some(), "the card shows a draft sample");

    // The batched deliveries are the card's detail rows — they must not also
    // surface as fifty entries in the needs-you queue.
    let needs_you = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM viryaos_autopilot_actions a \
         WHERE a.workspace_id = $1 AND a.status = 'awaiting_approval' \
           AND a.action_kind = 'community.engage.request' \
           AND a.payload->>'source_id' IS NULL",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(needs_you == 0, "batched deliveries are not loose asks");
    Ok(())
}

// ── The drip: one post per batch per interval, measured not scheduled ──

/// A succeeded engage action + its seeded pending community_post — the shape
/// the drip sees between a delivery's execution and its send.
async fn pending_delivery(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    target_id: Uuid,
    source_id: Uuid,
    subreddit: &str,
) -> Result<()> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_autopilot_decisions (
             id, workspace_id, decision_key, context, subject_kind, subject_id,
             decision_kind, confidence_basis_points, disposition, reason,
             input_snapshot, policy_snapshot, recommendation, trace_id
         ) VALUES ($1,$2,$3,'promotion_budget','target_community',$4,
                   'agent_content_proposal',7500,'require_approval','test',
                   '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,gen_random_uuid())",
    )
    .bind(decision_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("relay-pacing-{decision_id}"))
    .bind(target_id)
    .execute(pool)
    .await?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_autopilot_actions (
             id, workspace_id, decision_id, context, action_kind, subject_kind,
             subject_id, idempotency_key, payload, status, action_class,
             approved_at, approved_by, finished_at
         ) VALUES ($1,$2,$3,'promotion_budget','community.engage.request',
                   'target_community',$4,$5,$6,'succeeded','third_party',
                   now(),'operator:community_relay',now())",
    )
    .bind(action_id)
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(target_id)
    .bind(format!("relay-pacing-action-{action_id}"))
    .bind(json!({
        "kind": "request_community_engagement",
        "target_id": target_id,
        "platform": "reddit",
        "subreddit": subreddit,
        "title": "New video is out",
        "body": "what do you think",
        "source_id": source_id,
    }))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO community_posts \
             (workspace_id, action_id, target_id, subreddit, title, body, relay_source_id, status) \
         VALUES ($1,$2,$3,$4,'t','b',$5,'pending')",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(target_id)
    .bind(subreddit)
    .bind(source_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_drip_claims_one_post_per_batch_per_interval() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let source_id = content_source(&pool, ws).await?;

    // An approved batch — what the operator's one "yes" leaves behind.
    sqlx::query(
        "INSERT INTO community_relay_batches \
             (workspace_id, source_id, status, approved_at, approved_by, interval_seconds, observe_until) \
         VALUES ($1,$2,'approved',now(),'operator:community_relay',3600, now() + INTERVAL '7 days')",
    )
    .bind(ws.into_uuid())
    .bind(source_id)
    .execute(&pool)
    .await?;

    // Three pending deliveries of the same batch — the burst the interval
    // exists to prevent if the claim took every eligible row.
    for subreddit in ["metalpolska", "heavymetal", "deathmetal"] {
        let target = community_target(&pool, ws, subreddit).await?;
        pending_delivery(&pool, ws, target, source_id, subreddit).await?;
    }

    let executor = crowdrelay_worker::community_executor::CommunityExecutorWorker::new(
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

    // First sweep: one row of the batch claims, not all three.
    let claimed = executor.claim_pending_actions().await?;
    ensure!(
        claimed.len() == 1,
        "one delivery per batch per sweep, got {}",
        claimed.len()
    );

    // It went out. The batch's interval is measured from that post — the
    // next sweep immediately after must claim nothing, even though two
    // deliveries are pending and eligible by every other gate.
    sqlx::query(
        "UPDATE community_posts SET status='posted', posted_at=now() \
         WHERE workspace_id=$1 AND relay_source_id=$2 AND status='posting'",
    )
    .bind(ws.into_uuid())
    .bind(source_id)
    .execute(&pool)
    .await?;
    let claimed = executor.claim_pending_actions().await?;
    ensure!(
        claimed.is_empty(),
        "inside the interval nothing claims, got {}",
        claimed.len()
    );

    // A worker that slept past the interval posts the next one — downtime
    // delays the drip; it does not burst it.
    sqlx::query(
        "UPDATE community_posts SET posted_at = now() - INTERVAL '2 hours' \
         WHERE workspace_id=$1 AND relay_source_id=$2 AND status='posted'",
    )
    .bind(ws.into_uuid())
    .bind(source_id)
    .execute(&pool)
    .await?;
    let claimed = executor.claim_pending_actions().await?;
    ensure!(
        claimed.len() == 1,
        "after the interval the next delivery claims, got {}",
        claimed.len()
    );

    // And a revoked batch's leftovers never claim at all.
    sqlx::query(
        "UPDATE community_posts SET status='pending' \
         WHERE workspace_id=$1 AND relay_source_id=$2 AND status='posting'",
    )
    .bind(ws.into_uuid())
    .bind(source_id)
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE community_relay_batches SET status='revoked', revoked_at=now() \
         WHERE workspace_id=$1 AND source_id=$2",
    )
    .bind(ws.into_uuid())
    .bind(source_id)
    .execute(&pool)
    .await?;
    let claimed = executor.claim_pending_actions().await?;
    ensure!(
        claimed.is_empty(),
        "a revoked batch's pending rows never claim, got {}",
        claimed.len()
    );
    Ok(())
}

/// A delivery's queue state and the standing answer recorded behind it.
async fn delivery_state(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    target_id: Uuid,
) -> Result<(String, Option<String>)> {
    Ok(sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT status, approved_by FROM viryaos_autopilot_actions \
         WHERE workspace_id=$1 AND action_kind='community.engage.request' \
           AND payload->>'target_id' = $2::text",
    )
    .bind(workspace_id.into_uuid())
    .bind(target_id.to_string())
    .fetch_one(pool)
    .await?)
}

// ── Standing grants compose with the card ────────────────────────────

/// The two approvals are different questions: the grant is the operator's
/// standing "yes" to a community, the batch card is the "yes" to this
/// content for the rest of the spread. A granted draft queues and drips
/// without waiting on the card; the card's question stays open for the
/// communities nobody granted — and a revoked batch vetoes the content
/// even where a grant covers the target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_grant_posts_its_community_while_the_card_covers_the_rest() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let source_id = content_source(&pool, ws).await?;
    let granted = community_target(&pool, ws, "metalpolska").await?;
    let ungranted = community_target(&pool, ws, "heavymetal").await?;

    crowdrelay_infra::standing_approvals::grant(
        &pool,
        ws.into_uuid(),
        crowdrelay_infra::standing_approvals::GrantRequest {
            action_kind: "community.engage.request",
            target_key: &granted.to_string(),
            class: crowdrelay_domain::action_class::ActionClass::ThirdParty,
            granted_by: "operator:test",
            days: 90,
            note: None,
        },
        time::OffsetDateTime::now_utc(),
    )
    .await
    .context("grant the community")?;

    engage_outcome(&pool, ws, granted, source_id, "metalpolska").await?;
    engage_outcome(&pool, ws, ungranted, source_id, "heavymetal").await?;
    worker(&pool, ws).run_once().await?;

    // The grant answered for its community alone — the batch keeps asking
    // about the rest of the spread rather than inheriting one target's yes.
    let (status, _) = batch_row(&pool, ws, source_id)
        .await?
        .expect("the batch exists");
    ensure!(
        status == "awaiting_approval",
        "a per-target grant must not approve the whole spread, got {status}"
    );

    let (granted_status, granted_by) = delivery_state(&pool, ws, granted).await?;
    ensure!(
        granted_status == "queued",
        "the granted community's draft queues, got {granted_status}"
    );
    ensure!(
        granted_by.as_deref() == Some("standing_grant"),
        "the grant is the recorded answer, got {granted_by:?}"
    );
    let (other_status, _) = delivery_state(&pool, ws, ungranted).await?;
    ensure!(
        other_status == "awaiting_approval",
        "the ungranted community still waits on the card, got {other_status}"
    );

    // Both deliveries exist as posts once their actions execute; seed the
    // rows the executor seeds on success. The granted one is claimable
    // under the unanswered batch — its community is already a yes.
    for (target, subreddit) in [(granted, "metalpolska"), (ungranted, "heavymetal")] {
        pending_delivery(&pool, ws, target, source_id, subreddit).await?;
    }
    let executor = crowdrelay_worker::community_executor::CommunityExecutorWorker::new(
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
    ensure!(
        claimed.len() == 1,
        "only the grant-covered delivery claims under an unanswered card, got {}",
        claimed.len()
    );

    // The grant revoked mid-spread: the claimed row's community returns to
    // waiting on the card — nothing posts on a dead answer.
    crowdrelay_infra::standing_approvals::revoke(
        &pool,
        ws.into_uuid(),
        "community.engage.request",
        &granted.to_string(),
        "operator:test",
        time::OffsetDateTime::now_utc(),
    )
    .await
    .context("revoke the grant")?;
    sqlx::query(
        "UPDATE community_posts SET status='pending'          WHERE workspace_id=$1 AND relay_source_id=$2 AND status='posting'",
    )
    .bind(ws.into_uuid())
    .bind(source_id)
    .execute(&pool)
    .await?;
    let claimed = executor.claim_pending_actions().await?;
    ensure!(
        claimed.is_empty(),
        "a dead grant claims nothing — the card owns the question now, got {}",
        claimed.len()
    );

    // The card's answer covers both again.
    repository(&pool)
        .approve_community_relay(
            ws,
            source_id,
            None,
            None,
            &IdempotencyKey::parse("grant-card-yes")?,
            None,
        )
        .await
        .context("approve the card")?;
    let claimed = executor.claim_pending_actions().await?;
    ensure!(
        claimed.len() == 1,
        "an approved card resumes the drip for the rest, got {}",
        claimed.len()
    );
    Ok(())
}

/// A grant is an answer only while the question it answered is still asked.
/// The operator dialling `outreach` down to `observe` — or the class ceiling
/// below `require_approval` — retires the grant's say without touching its
/// row: the delivery stays parked rather than posting on a dead question.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_grant_stops_answering_once_the_axes_go_quiet() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let source_id = content_source(&pool, ws).await?;
    let granted = community_target(&pool, ws, "metalpolska").await?;

    crowdrelay_infra::standing_approvals::grant(
        &pool,
        ws.into_uuid(),
        crowdrelay_infra::standing_approvals::GrantRequest {
            action_kind: "community.engage.request",
            target_key: &granted.to_string(),
            class: crowdrelay_domain::action_class::ActionClass::ThirdParty,
            granted_by: "operator:test",
            days: 90,
            note: None,
        },
        time::OffsetDateTime::now_utc(),
    )
    .await
    .context("grant the community")?;

    engage_outcome(&pool, ws, granted, source_id, "metalpolska").await?;
    worker(&pool, ws).run_once().await?;
    pending_delivery(&pool, ws, granted, source_id, "metalpolska").await?;
    let executor = crowdrelay_worker::community_executor::CommunityExecutorWorker::new(
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

    // Sanity: with the seeded axes — outreach at require_approval, the
    // third-party ceiling no stricter — the live grant carries the row.
    let claimed = executor.claim_pending_actions().await?;
    ensure!(
        claimed.len() == 1,
        "the live grant claims under the unanswered card, got {}",
        claimed.len()
    );
    sqlx::query(
        "UPDATE community_posts SET status='pending', attempts = attempts - 1 \
         WHERE workspace_id=$1 AND relay_source_id=$2 AND status='posting'",
    )
    .bind(ws.into_uuid())
    .bind(source_id)
    .execute(&pool)
    .await?;

    // The context goes quiet. The grant row is still live — unrevoked,
    // unexpired — but there is no RequireApproval question left for it to
    // answer, so the delivery parks instead of posting on a dead question.
    sqlx::query(
        "UPDATE viryaos_autopilot_policies SET autonomy_level='observe' \
         WHERE workspace_id=$1 AND context='outreach'",
    )
    .bind(ws.into_uuid())
    .execute(&pool)
    .await?;
    let claimed = executor.claim_pending_actions().await?;
    ensure!(
        claimed.is_empty(),
        "a grant cannot answer a context that stopped asking, got {}",
        claimed.len()
    );

    // Context restored but the class ceiling tightened below the question —
    // the other axis going quiet must park the delivery just the same.
    sqlx::query(
        "UPDATE viryaos_autopilot_policies SET autonomy_level='require_approval' \
         WHERE workspace_id=$1 AND context='outreach'",
    )
    .bind(ws.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE viryaos_growth_autonomy SET ceiling='observe' \
         WHERE workspace_id=$1 AND action_class='third_party'",
    )
    .bind(ws.into_uuid())
    .execute(&pool)
    .await?;
    let claimed = executor.claim_pending_actions().await?;
    ensure!(
        claimed.is_empty(),
        "a ceiling below the question parks the grant's answer too, got {}",
        claimed.len()
    );

    // The ceiling lifts again — the grant answers once more, and the
    // refunded attempt means the parked churn cost the row nothing.
    sqlx::query(
        "UPDATE viryaos_growth_autonomy SET ceiling='require_approval' \
         WHERE workspace_id=$1 AND action_class='third_party'",
    )
    .bind(ws.into_uuid())
    .execute(&pool)
    .await?;
    let claimed = executor.claim_pending_actions().await?;
    ensure!(
        claimed.len() == 1,
        "axes restored, the live grant claims again, got {}",
        claimed.len()
    );
    Ok(())
}

// ── Operator edits at the approval gate ────────────────────────────
//
// The batch card's edit boxes land as `revisions` on the approval — each
// delivery's words reviewed through the same gate a single-action edit
// passes. A refused edit refuses the whole approval: the batch never
// approves around a draft the operator meant to fix.

async fn batch_action(pool: &PgPool, workspace_id: WorkspaceId, source_id: Uuid) -> Result<Uuid> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM viryaos_autopilot_actions \
         WHERE workspace_id = $1 AND action_kind = 'community.engage.request' \
           AND payload->>'source_id' = $2::text",
    )
    .bind(workspace_id.into_uuid())
    .bind(source_id)
    .fetch_one(pool)
    .await?)
}

async fn action_payload(pool: &PgPool, action_id: Uuid) -> Result<serde_json::Value> {
    Ok(sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT payload FROM viryaos_autopilot_actions WHERE id = $1",
    )
    .bind(action_id)
    .fetch_one(pool)
    .await?)
}

async fn seed_post_row(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    action_id: Uuid,
    source_id: Uuid,
    status: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO community_posts \
             (workspace_id, action_id, subreddit, title, body, relay_source_id, status) \
         VALUES ($1,$2,'metalpolska','seeded title','seeded body',$3,$4)",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(source_id)
    .bind(status)
    .execute(pool)
    .await
    .context("seed post row")?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn approving_with_revisions_rewrites_the_draft_and_audits_the_edit() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let source_id = content_source(&pool, ws).await?;
    let target_a = community_target(&pool, ws, "metalpolska").await?;
    let target_b = community_target(&pool, ws, "heavymetal").await?;
    engage_outcome(&pool, ws, target_a, source_id, "metalpolska").await?;
    engage_outcome(&pool, ws, target_b, source_id, "heavymetal").await?;
    worker(&pool, ws).run_once().await?;

    let actions = action_rows(&pool, ws).await?;
    let edited = actions[0].0;
    let untouched = actions[1].0;
    let revisions = std::collections::BTreeMap::from([(
        edited,
        std::collections::BTreeMap::from([
            ("title".to_owned(), "The fixed title".to_owned()),
            ("body".to_owned(), "The fixed body".to_owned()),
        ]),
    )]);
    let mutation = repository(&pool)
        .approve_community_relay(
            ws,
            source_id,
            None,
            Some(&revisions),
            &IdempotencyKey::parse("batch-approve-edit-1")?,
            None,
        )
        .await?;
    ensure!(
        mutation.status == "approved:2",
        "the spread still releases as one, got {}",
        mutation.status
    );

    let revised = action_payload(&pool, edited).await?;
    ensure!(
        revised["title"] == "The fixed title" && revised["body"] == "The fixed body",
        "the edited delivery carries the operator's words: {revised}"
    );
    let original = action_payload(&pool, untouched).await?;
    ensure!(
        original["title"] == "New video is out",
        "the delivery nobody edited keeps its draft: {original}"
    );
    // The released action queued carrying the revised text — the queue never
    // held an approved action with the unapproved words.
    let statuses = action_rows(&pool, ws).await?;
    ensure!(
        statuses.iter().all(|(_, status, _, _)| status == "queued"),
        "both deliveries released"
    );

    // The voice ledger: one row per edited field, the machine's words and
    // the operator's, keyed to this approval's operation.
    let rows = sqlx::query_as::<_, (String, String, String, Uuid)>(
        "SELECT field, before_text, after_text, operation_id \
         FROM viryaos_draft_revisions WHERE workspace_id = $1 AND action_id = $2 \
         ORDER BY field",
    )
    .bind(ws.into_uuid())
    .bind(edited)
    .fetch_all(&pool)
    .await?;
    ensure!(
        rows.len() == 2,
        "one audit row per edited field, got {rows:?}"
    );
    ensure!(
        rows[0]
            == (
                "body".to_owned(),
                "The band just dropped it — what do you think?".to_owned(),
                "The fixed body".to_owned(),
                mutation.operation_id
            ),
        "the body edit audits before → after under this approval, got {:?}",
        rows[0]
    );
    ensure!(
        rows[1].0 == "title"
            && rows[1].1 == "New video is out"
            && rows[1].2 == "The fixed title"
            && rows[1].3 == mutation.operation_id,
        "the title edit audits the same way, got {:?}",
        rows[1]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_revision_for_an_action_outside_the_batch_refuses_the_whole_approval() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let source_a = content_source(&pool, ws).await?;
    let source_b = content_source(&pool, ws).await?;
    let target_a = community_target(&pool, ws, "metalpolska").await?;
    let target_b = community_target(&pool, ws, "heavymetal").await?;
    engage_outcome(&pool, ws, target_a, source_a, "metalpolska").await?;
    engage_outcome(&pool, ws, target_b, source_b, "heavymetal").await?;
    worker(&pool, ws).run_once().await?;

    // The revision names source_b's delivery while approving source_a's
    // batch — an id the card cannot have shown for this batch. The map also
    // carries a *valid* edit for source_a's own delivery: it was drafted
    // first, so its v7 id sorts first in the map and its revision applies
    // before the refusal — the assertions below then prove the applied edit
    // rolled back, not merely that nothing ran.
    let own = batch_action(&pool, ws, source_a).await?;
    let foreign = batch_action(&pool, ws, source_b).await?;
    ensure!(
        own < foreign,
        "the own-delivery edit must apply before the refusal"
    );
    let revisions = std::collections::BTreeMap::from([
        (
            own,
            std::collections::BTreeMap::from([(
                "title".to_owned(),
                "applied then rolled back".to_owned(),
            )]),
        ),
        (
            foreign,
            std::collections::BTreeMap::from([("title".to_owned(), "edited".to_owned())]),
        ),
    ]);
    let refused = repository(&pool)
        .approve_community_relay(
            ws,
            source_a,
            None,
            Some(&revisions),
            &IdempotencyKey::parse("batch-approve-foreign-1")?,
            None,
        )
        .await;
    ensure!(refused.is_err(), "a foreign action id must refuse");

    // Nothing half-happened: batch_a still awaits, both drafts untouched,
    // and the applied edit's audit rows rolled back with it.
    let (status, _) = batch_row(&pool, ws, source_a).await?.expect("batch a");
    ensure!(
        status == "awaiting_approval",
        "the refusal is whole, got {status}"
    );
    for (id, status, _, _) in action_rows(&pool, ws).await? {
        let payload = action_payload(&pool, id).await?;
        ensure!(
            status == "awaiting_approval",
            "no delivery released, got {status}"
        );
        ensure!(
            payload["title"] == "New video is out",
            "no draft edited: {payload}"
        );
    }
    let audit_rows = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM viryaos_draft_revisions WHERE workspace_id = $1",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(
        audit_rows == 0,
        "the applied edit's ledger rows rolled back too"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_revision_to_a_fact_field_refuses_the_whole_approval() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let source_id = content_source(&pool, ws).await?;
    let target_a = community_target(&pool, ws, "metalpolska").await?;
    engage_outcome(&pool, ws, target_a, source_id, "metalpolska").await?;
    worker(&pool, ws).run_once().await?;

    let action_id = action_rows(&pool, ws).await?[0].0;
    // `subreddit` is where the post goes — the batch's fact, not its words.
    let revisions = std::collections::BTreeMap::from([(
        action_id,
        std::collections::BTreeMap::from([("subreddit".to_owned(), "othercommunity".to_owned())]),
    )]);
    let refused = repository(&pool)
        .approve_community_relay(
            ws,
            source_id,
            None,
            Some(&revisions),
            &IdempotencyKey::parse("batch-approve-badfield-1")?,
            None,
        )
        .await;
    ensure!(refused.is_err(), "a non-words field must refuse");
    let (status, _) = batch_row(&pool, ws, source_id).await?.expect("batch");
    ensure!(
        status == "awaiting_approval",
        "the refusal is whole, got {status}"
    );
    let payload = action_payload(&pool, action_id).await?;
    ensure!(
        payload["subreddit"] == "metalpolska",
        "the fact stayed put: {payload}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_seeded_post_row_takes_the_edit_but_a_post_out_the_door_refuses() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let source_id = content_source(&pool, ws).await?;
    let source_b = content_source(&pool, ws).await?;
    let target_a = community_target(&pool, ws, "metalpolska").await?;
    let target_b = community_target(&pool, ws, "heavymetal").await?;
    engage_outcome(&pool, ws, target_a, source_id, "metalpolska").await?;
    engage_outcome(&pool, ws, target_b, source_b, "heavymetal").await?;
    worker(&pool, ws).run_once().await?;
    let action_a = batch_action(&pool, ws, source_id).await?;
    let action_b = batch_action(&pool, ws, source_b).await?;

    // A grant-covered delivery is the real shape this guards: its action ran
    // (queued → processing → succeeded, the ledger's own ladder) and its
    // post row seeded ahead of the card's answer — so the seeded row is
    // what will send, and the edit must reach it too.
    for status in ["queued", "processing", "succeeded"] {
        // Terminal states require finished_at — the same invariant the
        // executor's own writes satisfy.
        sqlx::query(
            "UPDATE viryaos_autopilot_actions \
             SET status = $2, finished_at = CASE WHEN $2 IN ('succeeded','failed','cancelled') \
                 THEN now() ELSE finished_at END \
             WHERE id = $1",
        )
        .bind(action_a)
        .bind(status)
        .execute(&pool)
        .await
        .with_context(|| format!("step action to {status}"))?;
    }
    seed_post_row(&pool, ws, action_a, source_id, "pending").await?;
    let revisions = std::collections::BTreeMap::from([(
        action_a,
        std::collections::BTreeMap::from([("title".to_owned(), "the operator's title".to_owned())]),
    )]);
    repository(&pool)
        .approve_community_relay(
            ws,
            source_id,
            None,
            Some(&revisions),
            &IdempotencyKey::parse("batch-approve-seeded-1")?,
            None,
        )
        .await?;
    let post_title = sqlx::query_scalar::<_, String>(
        "SELECT title FROM community_posts WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(ws.into_uuid())
    .bind(action_a)
    .fetch_one(&pool)
    .await?;
    ensure!(
        post_title == "the operator's title",
        "the seeded row carries the edit, got {post_title}"
    );
    ensure!(
        action_payload(&pool, action_a).await?["title"] == "the operator's title",
        "the payload tells the same story"
    );

    // Once a post has left there is nothing to edit — the row on Reddit
    // keeps the words it left with, and the approval refuses loudly.
    seed_post_row(&pool, ws, action_b, source_b, "posted").await?;
    let revisions = std::collections::BTreeMap::from([(
        action_b,
        std::collections::BTreeMap::from([("title".to_owned(), "too late".to_owned())]),
    )]);
    let refused = repository(&pool)
        .approve_community_relay(
            ws,
            source_b,
            None,
            Some(&revisions),
            &IdempotencyKey::parse("batch-approve-posted-1")?,
            None,
        )
        .await;
    ensure!(refused.is_err(), "a post already out must refuse");
    let post_title = sqlx::query_scalar::<_, String>(
        "SELECT title FROM community_posts WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(ws.into_uuid())
    .bind(action_b)
    .fetch_one(&pool)
    .await?;
    ensure!(
        post_title == "seeded title",
        "the posted row kept its words"
    );
    let (status, _) = batch_row(&pool, ws, source_b).await?.expect("batch b");
    ensure!(
        status == "awaiting_approval",
        "the refusal is whole, got {status}"
    );
    Ok(())
}
