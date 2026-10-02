//! Regression tests for the community reply lane (`community_executor`'s
//! `run_reply_lane`), driven through the real worker against a migrated
//! suite database.
//!
//! Three regressions, all in the lane's SQL/policy wiring:
//!
//! 1. A draft the agents service keeps refusing never gave up: the backoff
//!    wrote `GREATEST(attempts, attempts)`, so the counter plateaued at one
//!    and `MAX_ATTEMPTS` was never reached — every persisted draft failure
//!    retried the comment every thirty minutes forever instead of surfacing
//!    it as `failed`.
//! 2. The Reddit send gate counted `replied` rows on every platform, so a
//!    busy Instagram thread spent the Reddit lane's daily ceiling and reset
//!    its spacing — Reddit replies starved while owned replies flowed.
//! 3. `review_community_register` — scoped by its own contract to the
//!    community channel — also held Instagram and Facebook drafts for
//!    hashtags or a call to action the band's own audience opted into.

use crate::common;

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_worker::community_executor::CommunityExecutorWorker;
use sqlx::PgPool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

/// One JSON body per request path — the smallest agents service that still
/// exercises the real HTTP/JSON path the worker takes.
async fn serve_agents(listener: TcpListener) {
    while let Ok((mut stream, _)) = listener.accept().await {
        let mut buffer = [0_u8; 16 * 1024];
        let read = match stream.read(&mut buffer).await {
            Ok(read) => read,
            Err(_) => continue,
        };
        let request = String::from_utf8_lossy(&buffer[..read]);
        let path = request
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_owned();
        let body = match path.as_str() {
            "/community/reply-draft" => {
                r#"{"reply":"dzięki za miłe słowa, widzimy się w Gorzowie! #virya","provider":"mock","model":"mock"}"#
            }
            "/community/review" => r#"{"score":8.0,"pass":true}"#,
            _ => r#"{"error":"unknown"}"#,
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
    }
}

/// Same wire path, but a newer drafter explicitly identifies a commenter who
/// asked how to stay connected. The reply itself remains conversational and
/// link-free; CrowdRelay decides whether to offer the tracked owned link.
async fn serve_capture_agents(listener: TcpListener) {
    while let Ok((mut stream, _)) = listener.accept().await {
        let mut buffer = [0_u8; 16 * 1024];
        let read = match stream.read(&mut buffer).await {
            Ok(read) => read,
            Err(_) => continue,
        };
        let request = String::from_utf8_lossy(&buffer[..read]);
        let path = request
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_owned();
        let body = match path.as_str() {
            "/community/reply-draft" => {
                r#"{"reply":"Jasne — dzięki, że pytasz.","capture_intent":"none","capture_evidence":null,"provider":"mock","model":"mock"}"#
            }
            "/community/review" => r#"{"score":9.0,"pass":true}"#,
            _ => r#"{"error":"unknown"}"#,
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
    }
}

async fn workspace(pool: &PgPool) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("replies-{}", id.simple()))
        .bind("Replies")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(WorkspaceId::from_uuid(id))
}

/// The decision → action → community_post chain a reddit comment's
/// `community_post_id` foreign key needs.
async fn community_post(pool: &PgPool, workspace_id: WorkspaceId) -> Result<Uuid> {
    let ws = workspace_id.into_uuid();
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
    .bind(ws)
    .bind(format!("engage-{decision_id}"))
    .bind(ws)
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert decision")?;
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
    .bind(ws)
    .bind(decision_id)
    .bind(ws)
    .bind(format!("action-{action_id}"))
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .context("insert action")?;
    let post_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO community_posts
            (id, workspace_id, action_id, subreddit, title, body, status, posted_at)
        VALUES ($1,$2,$3,'r/test','title','body','posted', now() - interval '2 days')
        "#,
    )
    .bind(post_id)
    .bind(ws)
    .bind(action_id)
    .execute(pool)
    .await
    .context("insert community post")?;
    Ok(post_id)
}

/// The synced social post an owned-channel comment's `content_source_id`
/// foreign key needs.
async fn social_source(pool: &PgPool, workspace_id: WorkspaceId) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO content_sources
            (id, workspace_id, source_kind, source_key, title, occurred_at,
             expires_at, metadata)
        VALUES ($1,$2,'social_post',$3,'post', now() - interval '1 day',
                now() + interval '30 days',
                '{"platform":"instagram","comments_count":3}'::jsonb)
        "#,
    )
    .bind(id)
    .bind(workspace_id.into_uuid())
    .bind(format!("instagram:{}", id.simple()))
    .execute(pool)
    .await
    .context("insert social content source")?;
    Ok(id)
}

fn worker(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    manual_mode: bool,
    agents_url: &str,
) -> Result<CommunityExecutorWorker> {
    CommunityExecutorWorker::new(
        pool.clone(),
        workspace_id,
        Duration::from_secs(10),
        manual_mode,
        None,
        agents_url.to_owned(),
        Some("test-agents-key".to_owned()),
        None,
    )
    .context("build community executor")
}

/// A draft the agents service can never serve must count its attempts and
/// give up — not retry every backoff interval forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_failing_draft_spends_its_attempts_and_gives_up() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .context("connect to the migrated suite database")?;
    let ws = workspace(&pool).await?;
    let post_id = community_post(&pool, ws).await?;
    sqlx::query(
        r#"
        INSERT INTO community_comments
            (workspace_id, platform, community_post_id, platform_comment_id,
             parent_id, author, body, status)
        VALUES ($1,'reddit',$2,'t1_aaa','t3_bbb','fan','kiedy koncert?','unanswered')
        "#,
    )
    .bind(ws.into_uuid())
    .bind(post_id)
    .execute(&pool)
    .await
    .context("insert unanswered comment")?;

    // Nothing listens on port 1: every draft call fails, which is exactly the
    // state "the agents service keeps refusing this draft".
    let worker = worker(&pool, ws, true, "http://127.0.0.1:1")?;
    for round in 1..=5 {
        worker.run_reply_lane().await?;
        sqlx::query(
            "UPDATE community_comments SET not_before = NULL \
             WHERE workspace_id = $1 AND status = 'unanswered'",
        )
        .bind(ws.into_uuid())
        .execute(&pool)
        .await?;
        let (status, attempts): (String, i32) = sqlx::query_as(
            "SELECT status, attempts FROM community_comments WHERE workspace_id = $1",
        )
        .bind(ws.into_uuid())
        .fetch_one(&pool)
        .await?;
        if round < 5 {
            ensure!(
                status == "unanswered" && attempts == round,
                "after {round} failures a draft must hold attempt {round} for the \
                 next try, got status={status} attempts={attempts}"
            );
        }
    }
    let (status, attempts): (String, i32) =
        sqlx::query_as("SELECT status, attempts FROM community_comments WHERE workspace_id = $1")
            .bind(ws.into_uuid())
            .fetch_one(&pool)
            .await?;
    ensure!(
        status == "failed" && attempts == 5,
        "five refused drafts must surface as failed, got status={status} \
         attempts={attempts}"
    );
    Ok(())
}

/// A Reddit reply's daily ceiling and spacing measure the Reddit lane — not
/// whatever the band's own Instagram and Facebook did.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn owned_replies_do_not_spend_the_reddit_ceiling() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .context("connect to the migrated suite database")?;
    // The send lane needs the write switch; restore whatever the process had.
    let previous = std::env::var("CROWDRELAY_REDDIT_WRITE_ENABLED").ok();
    unsafe { std::env::set_var("CROWDRELAY_REDDIT_WRITE_ENABLED", "true") };
    let result = reddit_send_ignores_owned_replies(&pool).await;
    unsafe {
        match previous {
            Some(value) => std::env::set_var("CROWDRELAY_REDDIT_WRITE_ENABLED", value),
            None => std::env::remove_var("CROWDRELAY_REDDIT_WRITE_ENABLED"),
        }
    }
    result
}

async fn reddit_send_ignores_owned_replies(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    let source_id = social_source(pool, ws).await?;
    // Twelve owned-channel replies just went out — the owned lane's own
    // budget, and also Reddit's whole daily ceiling if the lanes share one.
    for n in 0..12 {
        sqlx::query(
            r#"
            INSERT INTO community_comments
                (workspace_id, platform, content_source_id, platform_comment_id,
                 parent_id, author, body, status, draft, reply_comment_id,
                 replied_at)
            VALUES ($1,'instagram',$2,$3,'555','virya','dzięki!','replied',
                    'dzięki!',$4, now())
            "#,
        )
        .bind(ws.into_uuid())
        .bind(source_id)
        .bind(format!("70{n}"))
        .bind(format!("80{n}"))
        .execute(pool)
        .await
        .context("insert replied instagram comment")?;
    }
    let post_id = community_post(pool, ws).await?;
    let reddit_comment = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO community_comments
            (id, workspace_id, platform, community_post_id, platform_comment_id,
             parent_id, author, body, status, draft, approved_by)
        VALUES ($1,$2,'reddit',$3,'t1_ccc','t3_ddd','fan','nice set',
                'approved','widzimy się!','unattended: test')
        "#,
    )
    .bind(reddit_comment)
    .bind(ws.into_uuid())
    .bind(post_id)
    .execute(pool)
    .await
    .context("insert approved reddit reply")?;

    // A live send lane (write switch on, manual mode off) against a dead
    // agents service: the claim must run, the send fails, the row defers.
    let worker = worker(pool, ws, false, "http://127.0.0.1:1")?;
    worker.run_reply_lane().await?;

    let (status, attempts): (String, i32) =
        sqlx::query_as("SELECT status, attempts FROM community_comments WHERE id = $1")
            .bind(reddit_comment)
            .fetch_one(pool)
            .await?;
    ensure!(
        attempts == 1,
        "owned replies must not consume the reddit ceiling — the approved \
         reply should have been claimed (attempts=1), got attempts={attempts}"
    );
    ensure!(
        status == "approved",
        "a deferred send returns to approved, got {status}"
    );
    Ok(())
}

/// The community register guard — hashtags, sales lines, generated phrasing —
/// is the community channel's own: the band's Instagram audience opted into a
/// hashtag. An owned-channel draft must route on the publish guard and the
/// review, never on "on Reddit they read as cross-posted marketing".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_register_guard_does_not_govern_owned_replies() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .context("connect to the migrated suite database")?;
    let listener = TcpListener::bind((IpAddr::from([127, 0, 0, 1]), 0))
        .await
        .context("bind mock agents service")?;
    let agents_url = format!(
        "http://{}",
        listener.local_addr().context("read listener address")?
    );
    let server = tokio::spawn(serve_agents(listener));

    let result = async {
        let ws = workspace(&pool).await?;
        let source_id = social_source(&pool, ws).await?;
        sqlx::query(
            r#"
            INSERT INTO community_comments
                (workspace_id, platform, content_source_id, platform_comment_id,
                 parent_id, author, body, status)
            VALUES ($1,'instagram',$2,'901','555','fan1','Kiedy gracie Wrocław?',
                    'unanswered')
            "#,
        )
        .bind(ws.into_uuid())
        .bind(source_id)
        .execute(&pool)
        .await
        .context("insert unanswered instagram comment")?;

        let worker = worker(&pool, ws, true, &agents_url)?;
        worker.run_reply_lane().await?;

        let (status, hold_reason): (String, Option<String>) = sqlx::query_as(
            "SELECT status, hold_reason FROM community_comments \
             WHERE workspace_id = $1",
        )
        .bind(ws.into_uuid())
        .fetch_one(&pool)
        .await?;
        ensure!(
            status == "awaiting_approval",
            "a clean, review-passed draft waits for a person, got {status}"
        );
        ensure!(
            hold_reason.is_none(),
            "the band's own channel does not answer to the community \
             register — got hold_reason={hold_reason:?}"
        );
        Ok(())
    }
    .await;
    server.abort();
    result
}

/// Explicit follow/join evidence becomes a measured capture opportunity even
/// when the copy model says capture_intent=none: typed FAN SCOUT policy owns
/// the CTA. The tracked invitation is still held for a person before send.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn fan_scout_not_the_copy_model_owns_the_tracked_capture_decision() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .context("connect to the migrated suite database")?;
    let listener = TcpListener::bind((IpAddr::from([127, 0, 0, 1]), 0))
        .await
        .context("bind capture mock agents service")?;
    let agents_url = format!(
        "http://{}",
        listener.local_addr().context("read listener address")?
    );
    let server = tokio::spawn(serve_capture_agents(listener));

    let result = async {
        let ws = workspace(&pool).await?;
        sqlx::query(
            "INSERT INTO tenant_settings (workspace_id, key, value)
             VALUES ($1, 'member_site_base_url', 'https://band.example')",
        )
        .bind(ws.into_uuid())
        .execute(&pool)
        .await
        .context("configure member site")?;

        let source_id = social_source(&pool, ws).await?;
        let comment_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO community_comments
                (id, workspace_id, platform, content_source_id,
                 platform_comment_id, parent_id, author, body, status)
            VALUES ($1,$2,'instagram',$3,'991','555','fan1',
                    'gdzie mogę was śledzić?','unanswered')
            "#,
        )
        .bind(comment_id)
        .bind(ws.into_uuid())
        .bind(source_id)
        .execute(&pool)
        .await
        .context("insert explicit follow intent")?;

        let worker = worker(&pool, ws, true, &agents_url)?;
        worker.run_reply_lane().await?;

        let (status, draft, hold_reason): (String, Option<String>, Option<String>) =
            sqlx::query_as(
                "SELECT status, draft, hold_reason
                 FROM community_comments WHERE id = $1",
            )
            .bind(comment_id)
            .fetch_one(&pool)
            .await?;
        ensure!(
            status == "awaiting_approval",
            "a capture CTA must never leave unattended, got {status}"
        );
        let draft = draft.context("capture reply should have a draft")?;
        let expected_slug = format!("reply-capture-{}", comment_id.simple());
        ensure!(
            draft.contains(&format!("/l/{expected_slug}")),
            "Brain-selected InviteToFanbase should attach the exact tracked CTA even though the model requested none: {draft:?}"
        );
        ensure!(
            hold_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("FAN SCOUT selected InviteToFanbase")),
            "the queue should explain the FAN SCOUT invite decision, got {hold_reason:?}"
        );

        let (slug, destination, source, community, creative): (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT slug, destination_url, channel_source, channel_community,
                    channel_creative
             FROM smart_links
             WHERE workspace_id = $1 AND slug = $2",
        )
        .bind(ws.into_uuid())
        .bind(&expected_slug)
        .fetch_one(&pool)
        .await?;
        ensure!(slug == expected_slug);
        ensure!(
            destination.starts_with(
                "https://band.example/signal?utm_source=instagram&utm_medium=comment_reply"
            ),
            "capture stays tenant-native and source-tagged: {destination}"
        );
        ensure!(source.as_deref() == Some("instagram"));
        let expected_community = format!("comment:{comment_id}");
        ensure!(community.as_deref() == Some(expected_community.as_str()));
        ensure!(creative.as_deref() == Some("owned_reply_capture"));
        Ok(())
    }
    .await;

    server.abort();
    result
}
