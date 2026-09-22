//! The strategy-consult loop, end to end against a real database.
//!
//! A `strategy_proposals` outcome is advice, not authority: the worker
//! evaluates each typed proposal deterministically and writes one verdict
//! row per proposal — accepted and implemented through a bounded channel,
//! or rejected with the reason the consultant reads back next week.
//!
//! Also covered here because it shares the outcome path: a non-Reddit
//! community target — the scout's "add as a fanbase to join" — files its
//! place under `platform` + `community_url`, dedupes on the normalized URL,
//! and is screened exactly like a subreddit.

use crate::common;

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_worker::agent_outcomes::AgentOutcomeWorker;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

async fn workspace(pool: &PgPool) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("strategy-{}", id.simple()))
        .bind("Strategy Test")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(WorkspaceId::from_uuid(id))
}

/// Inserts an outcome row directly, as the agents service would — with the
/// provenance block `require_approval` kinds need to pass admission.
async fn insert_outcome(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    kind: &str,
    payload: serde_json::Value,
) -> Result<Uuid> {
    let mut payload = payload;
    if let Some(obj) = payload.as_object_mut() {
        obj.entry("provenance").or_insert(json!({
            "verification": { "status": "grounding_check_passed" },
            "context": { "any_source_failed": false, "any_source_truncated": false },
            "confidence": { "basis_points": 8000, "source": "model_self_report", "is_evidence_confidence": false },
            "model": { "actual": "test-model", "provider": "test" }
        }));
    }
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO agent_outcomes (
            id, workspace_id, task_id, result_id, kind, schema_version,
            payload, confidence_basis_points, idempotency_key, status
        ) VALUES ($1,$2,$3,$4,$5,1,$6,8000,$7,'pending')
        "#,
    )
    .bind(id)
    .bind(workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .bind(kind)
    .bind(&payload)
    .bind(format!("strategy-test-{id}"))
    .execute(pool)
    .await
    .context("insert outcome")?;
    Ok(id)
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

async fn verdicts(
    pool: &PgPool,
    workspace_id: WorkspaceId,
) -> Result<Vec<(String, String, String, Option<String>)>> {
    Ok(sqlx::query_as(
        "SELECT action, verdict, reason, implemented_as \
         FROM agent_strategy_proposal_verdicts WHERE workspace_id = $1 \
         ORDER BY proposal_index",
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await?)
}

fn proposal_item(proposals: serde_json::Value) -> serde_json::Value {
    json!({
        "type": "strategy_proposal",
        "headline": "Grow through scenes, not feeds",
        "detail": "The existing cadence under-uses community discovery.",
        "proposals": proposals,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_proposals_outcome_writes_a_verdict_per_proposal() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;

    insert_outcome(
        &pool,
        ws,
        "strategy_proposals",
        json!({
            "item": proposal_item(json!([
                { "action": "queue_scan_queries",
                  "rationale": "cover the local scene",
                  "queries": ["deathcore forum uk", "deathcore forum uk", ""] },
                { "action": "rescan", "template_id": "reddit-scanner",
                  "rationale": "new subreddits landed" },
                { "action": "rescan", "template_id": "social-post",
                  "rationale": "post more" },
                { "action": "adjust_cadence", "template_id": "fanbase-scout",
                  "cooldown_hours": 96, "rationale": "weekly is too slow" },
                { "action": "adjust_cadence", "template_id": "social-post",
                  "cooldown_hours": 2, "rationale": "hourly posting" },
                { "action": "not_a_real_action", "rationale": "invented" },
                { "action": "surface_to_operator",
                  "rationale": "a human should weigh this" },
            ])),
            "rationale": "weekly consult",
        }),
    )
    .await?;

    worker(&pool, ws).run_once().await?;

    let rows = verdicts(&pool, ws).await?;
    ensure!(
        rows.len() == 7,
        "every proposal lands a verdict, got {rows:?}"
    );

    let (action, verdict, _reason, implemented) = &rows[0];
    ensure!(action == "queue_scan_queries" && verdict == "accepted");
    ensure!(
        implemented.as_deref() == Some("queued 1 scan queries"),
        "one of three query strings was a dupe and one empty — got {implemented:?}"
    );
    let queued: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_scan_query_queue WHERE workspace_id = $1")
            .bind(ws.into_uuid())
            .fetch_one(&pool)
            .await?;
    ensure!(
        queued == 1,
        "the deduped query must land once, got {queued}"
    );

    let (action, verdict, _, implemented) = &rows[1];
    ensure!(action == "rescan" && verdict == "accepted");
    ensure!(
        implemented
            .as_deref()
            .is_some_and(|s| s.starts_with("rescan request")),
        "rescan must record its request row, got {implemented:?}"
    );
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_template_rescan_requests \
         WHERE workspace_id = $1 AND template_id = 'reddit-scanner' AND consumed_at IS NULL",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(
        pending == 1,
        "the rescan must sit pending for the dispatcher"
    );

    let (_, verdict, reason, _) = &rows[2];
    ensure!(
        verdict == "rejected" && reason.contains("intelligence templates"),
        "a rescan on a posting worker must reject — got {verdict} {reason}"
    );

    let (_, verdict, _, implemented) = &rows[3];
    ensure!(verdict == "accepted");
    ensure!(
        implemented.as_deref() == Some("policy fanbase_scout_cooldown_hours=96h"),
        "the cadence write must name the field it set — got {implemented:?}"
    );
    let stored: i64 = sqlx::query_scalar(
        "SELECT (config->>'fanbase_scout_cooldown_hours')::bigint \
         FROM autopilot_policies WHERE workspace_id = $1 AND context = 'growth_intelligence'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(
        stored == 96,
        "the policy config must hold the new cooldown, got {stored}"
    );

    let (_, verdict, reason, _) = &rows[4];
    ensure!(
        verdict == "rejected" && reason.contains("outside the"),
        "a cadence below the floor must reject — got {verdict} {reason}"
    );

    let (_, verdict, reason, _) = &rows[5];
    ensure!(
        verdict == "rejected" && reason.contains("unknown action"),
        "an action outside the vocabulary must reject — got {verdict} {reason}"
    );

    let (_, verdict, _, implemented) = &rows[6];
    ensure!(verdict == "accepted" && implemented.as_deref() == Some("surfaced"));

    // Verdicts are the record — a strategy outcome creates no queued action
    // row, same as a scout finding.
    let actions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM autopilot_actions WHERE workspace_id = $1")
            .bind(ws.into_uuid())
            .fetch_one(&pool)
            .await?;
    ensure!(actions == 0, "a consultation must create no action rows");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_proposals_outcome_with_no_proposals_is_rejected() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;

    let outcome_id = insert_outcome(
        &pool,
        ws,
        "strategy_proposals",
        json!({
            "item": {
                "type": "strategy_proposal",
                "headline": "No ideas",
                "detail": "Nothing to change.",
                "proposals": [],
            },
            "rationale": "weekly consult",
        }),
    )
    .await?;

    worker(&pool, ws).run_once().await?;

    let reason: Option<String> =
        sqlx::query_scalar("SELECT rejection_reason FROM agent_outcomes WHERE id = $1")
            .bind(outcome_id)
            .fetch_one(&pool)
            .await?;
    ensure!(
        reason
            .as_deref()
            .is_some_and(|r| r.contains("MISSING_PROPOSAL_CONTENT")),
        "an empty consultation must reject — got {reason:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_proposed_community_lands_as_a_screened_place_and_target() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let slug = Uuid::now_v7().simple().to_string();
    let url = format!("https://discord.gg/{slug}");

    insert_outcome(
        &pool,
        ws,
        "strategy_proposals",
        json!({
            "item": proposal_item(json!([
                { "action": "propose_community",
                  "rationale": "their fans live here",
                  "community": {
                      "platform": "discord",
                      "name": "Deathcore Central",
                      "url": url,
                      "language": "en",
                      "why": "active scene server" } },
                { "action": "propose_community",
                  "rationale": "vague",
                  "community": { "platform": "myspace", "name": "Old", "url": "https://x.example" } },
            ])),
            "rationale": "weekly consult",
        }),
    )
    .await?;

    worker(&pool, ws).run_once().await?;

    let rows = verdicts(&pool, ws).await?;
    ensure!(rows.len() == 2, "got {rows:?}");
    ensure!(
        rows[0].1 == "accepted",
        "a real community proposal must accept — {rows:?}"
    );
    ensure!(
        rows[1].1 == "rejected" && rows[1].2.contains("place vocabulary"),
        "an unknown platform must reject — {rows:?}"
    );

    // The accepted community is a place on the audience graph AND a screened
    // outreach target — the two rows the rest of the machinery reads.
    let place: Option<(String, String)> = sqlx::query_as(
        "SELECT place_kind, platform FROM discovery_places \
         WHERE workspace_id = $1 AND url = $2",
    )
    .bind(ws.into_uuid())
    .bind(&url)
    .fetch_optional(&pool)
    .await?;
    let Some((kind, platform)) = place else {
        anyhow::bail!("the proposed community must land as a discovery place");
    };
    ensure!(kind == "discord" && platform == "discord");

    let target: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT status, screening_verdict FROM agent_outreach_targets \
         WHERE workspace_id = $1 AND community_url IS NOT NULL",
    )
    .bind(ws.into_uuid())
    .fetch_optional(&pool)
    .await?;
    let Some((status, verdict)) = target else {
        anyhow::bail!("the proposed community must land as an outreach target");
    };
    ensure!(
        status == "promoted",
        "a community target auto-promotes — got {status}"
    );
    ensure!(
        verdict.as_deref().is_some(),
        "the community must carry a screening verdict"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_non_reddit_community_target_dedupes_on_the_normalized_url() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let slug = Uuid::now_v7().simple().to_string();

    for (name, url) in [
        (
            "Deathcore Forum",
            format!("https://forum.example/board/{slug}"),
        ),
        (
            "Deathcore Forum — renamed",
            format!("https://www.forum.example/board/{slug}/"),
        ),
    ] {
        insert_outcome(
            &pool,
            ws,
            "outreach_targets",
            json!({
                "item": {
                    "type": "outreach_target",
                    "target_kind": "community",
                    "display_name": name,
                    "platform": "forum",
                    "community_url": url,
                    "why_fit": "scene forum",
                    "evidence_urls": [url],
                    "language": "en",
                },
                "rationale": "fanbase scout",
            }),
        )
        .await?;
    }

    worker(&pool, ws).run_once().await?;

    let targets: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_outreach_targets \
         WHERE workspace_id = $1 AND target_kind = 'community'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(
        targets == 1,
        "scheme/www/trailing-slash variants are one community — got {targets} rows"
    );
    Ok(())
}

// A reddit community proposed by URL instead of a `subreddit` field folds
// onto subreddit identity — the scout's contract asks for the field, but a
// model that answers with only the link must still dedupe and file like
// every other subreddit proposal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_reddit_url_without_a_subreddit_field_is_still_one_subreddit() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    let slug = format!("r{}", &Uuid::now_v7().simple().to_string()[..12]);

    for (name, item) in [
        (
            "by field",
            json!({
                "type": "outreach_target",
                "target_kind": "community",
                "platform": "reddit",
                "subreddit": slug,
                "why_fit": "scene sub",
                "evidence_urls": [format!("https://reddit.com/r/{slug}")],
            }),
        ),
        (
            "by url",
            json!({
                "type": "outreach_target",
                "target_kind": "community",
                "platform": "reddit",
                "community_url": format!("https://www.reddit.com/r/{}/", slug.to_uppercase()),
                "why_fit": "scene sub",
                "evidence_urls": [format!("https://reddit.com/r/{slug}")],
            }),
        ),
    ] {
        let mut item = item;
        item["display_name"] = json!(name);
        insert_outcome(
            &pool,
            ws,
            "outreach_targets",
            json!({ "item": item, "rationale": "fanbase scout" }),
        )
        .await?;
    }

    worker(&pool, ws).run_once().await?;

    let rows: Vec<(Option<String>, String)> = sqlx::query_as(
        "SELECT subreddit, status FROM agent_outreach_targets \
         WHERE workspace_id = $1 AND target_kind = 'community'",
    )
    .bind(ws.into_uuid())
    .fetch_all(&pool)
    .await?;
    ensure!(
        rows.len() == 1 && rows[0].0.as_deref() == Some(slug.as_str()),
        "a subreddit named by URL is the same subreddit — got {rows:?}"
    );
    let place: Option<(String,)> = sqlx::query_as(
        "SELECT url FROM discovery_places \
         WHERE workspace_id = $1 AND place_kind = 'subreddit'",
    )
    .bind(ws.into_uuid())
    .fetch_optional(&pool)
    .await?;
    ensure!(
        place.as_ref().map(|(u,)| u.as_str())
            == Some(format!("https://www.reddit.com/r/{slug}").as_str()),
        "the place carries the canonical subreddit url — got {place:?}"
    );
    Ok(())
}

// The URL identity folds host case but not path case: invite codes are
// case-sensitive, so /AbC and /abc are two different rooms and merging them
// would join the wrong community.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn case_sensitive_invite_paths_stay_distinct() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;
    // Fixed codes: the pair must differ by case deterministically, which a
    // random hex invite cannot guarantee.
    for url in ["https://DISCORD.gg/AbCd1234", "https://discord.gg/abcd1234"] {
        insert_outcome(
            &pool,
            ws,
            "outreach_targets",
            json!({
                "item": {
                    "type": "outreach_target",
                    "target_kind": "community",
                    "display_name": "Discord",
                    "platform": "discord",
                    "community_url": url,
                    "why_fit": "scene server",
                    "evidence_urls": [url],
                },
                "rationale": "fanbase scout",
            }),
        )
        .await?;
    }

    worker(&pool, ws).run_once().await?;

    let targets: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_outreach_targets \
         WHERE workspace_id = $1 AND target_kind = 'community'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(
        targets == 2,
        "host case folds but invite-path case does not — got {targets} rows"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_whitespace_action_lands_a_verdict_not_an_outcome_failure() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;

    let outcome_id = insert_outcome(
        &pool,
        ws,
        "strategy_proposals",
        json!({
            "item": proposal_item(json!([
                { "action": "   ", "rationale": "blank action" },
                { "action": "queue_scan_queries",
                  "rationale": "one real ask", "queries": ["wroclaw metal forum"] },
            ])),
            "rationale": "weekly consult",
        }),
    )
    .await?;

    worker(&pool, ws).run_once().await?;

    let rows = verdicts(&pool, ws).await?;
    ensure!(
        rows.len() == 2,
        "both proposals must land verdicts — got {rows:?}"
    );
    let (action, verdict, reason, _) = &rows[0];
    ensure!(
        action == "unknown" && verdict == "rejected" && reason.contains("unknown action"),
        "a whitespace action becomes a rejected verdict — got {action} {verdict} {reason}"
    );
    ensure!(
        rows[1].1 == "accepted",
        "the sibling proposal must survive — got {:?}",
        rows[1]
    );
    let outcome_status: String =
        sqlx::query_scalar("SELECT status FROM agent_outcomes WHERE id = $1")
            .bind(outcome_id)
            .fetch_one(&pool)
            .await?;
    ensure!(
        outcome_status != "rejected",
        "the outcome itself must not fail — got {outcome_status}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_second_rescan_inside_the_floor_is_rejected() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;

    // First consult queues the rescan; the dispatcher consumes it, and the
    // next consult asks again — the floor must reject the second ask or the
    // one-shot bypass becomes a standing hourly loop.
    insert_outcome(
        &pool,
        ws,
        "strategy_proposals",
        json!({
            "item": proposal_item(json!([
                { "action": "rescan", "template_id": "reddit-scanner",
                  "rationale": "fresh scrape needed" },
            ])),
            "rationale": "weekly consult",
        }),
    )
    .await?;
    worker(&pool, ws).run_once().await?;
    sqlx::query(
        "UPDATE agent_template_rescan_requests SET consumed_at = now()
         WHERE workspace_id = $1 AND consumed_at IS NULL",
    )
    .bind(ws.into_uuid())
    .execute(&pool)
    .await?;

    insert_outcome(
        &pool,
        ws,
        "strategy_proposals",
        json!({
            "item": proposal_item(json!([
                { "action": "rescan", "template_id": "reddit-scanner",
                  "rationale": "do it again" },
            ])),
            "rationale": "weekly consult",
        }),
    )
    .await?;
    worker(&pool, ws).run_once().await?;

    let rows = verdicts(&pool, ws).await?;
    ensure!(rows.len() == 2, "one verdict per proposal — got {rows:?}");
    ensure!(rows[0].1 == "accepted");
    ensure!(
        rows[1].1 == "rejected" && rows[1].2.contains("within the last"),
        "the second ask inside the floor must reject — got {:?}",
        rows[1]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_discarded_community_stays_discarded_and_the_verdict_says_so() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = workspace(&pool).await?;

    insert_outcome(
        &pool,
        ws,
        "strategy_proposals",
        json!({
            "item": proposal_item(json!([
                { "action": "propose_community", "rationale": "join the scene",
                  "community": { "platform": "discord", "name": "Deathcore",
                                 "url": "https://discord.gg/deathcore-scene" } },
            ])),
            "rationale": "weekly consult",
        }),
    )
    .await?;
    worker(&pool, ws).run_once().await?;

    sqlx::query(
        "UPDATE agent_outreach_targets SET status = 'discarded'
         WHERE workspace_id = $1 AND target_kind = 'community'",
    )
    .bind(ws.into_uuid())
    .execute(&pool)
    .await?;

    insert_outcome(
        &pool,
        ws,
        "strategy_proposals",
        json!({
            "item": proposal_item(json!([
                { "action": "propose_community", "rationale": "join the scene",
                  "community": { "platform": "discord", "name": "Deathcore",
                                 "url": "https://discord.gg/deathcore-scene" } },
            ])),
            "rationale": "weekly consult",
        }),
    )
    .await?;
    worker(&pool, ws).run_once().await?;

    let rows = verdicts(&pool, ws).await?;
    ensure!(rows.len() == 2, "one verdict per proposal — got {rows:?}");
    ensure!(
        rows[1].1 == "rejected" && rows[1].2.contains("discarded"),
        "the re-proposal must answer 'discarded' — got {:?}",
        rows[1]
    );
    let status: String = sqlx::query_scalar(
        "SELECT status FROM agent_outreach_targets
         WHERE workspace_id = $1 AND target_kind = 'community'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(
        status == "discarded",
        "the row must stay discarded — got {status}"
    );
    Ok(())
}

/// A community screening refused is not joinable, whatever its place row
/// says. The scout files the place before the verdict exists, so a refused
/// place still reads 'active' + 'not_joined' — the claim must gate on the
/// refused target, not on the place alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_refused_community_is_never_claimed_for_joining() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool).await?;
    let ws_uuid = ws.into_uuid();

    let place_refused: Uuid = sqlx::query_scalar(
        "INSERT INTO discovery_places
            (workspace_id, place_kind, platform, name, url, status, membership_state)
         VALUES ($1, 'subreddit', 'reddit', 'r/offtopicscene', $2, 'active', 'not_joined')
         RETURNING id",
    )
    .bind(ws_uuid)
    .bind("https://www.reddit.com/r/offtopicscene")
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO agent_outreach_targets
            (workspace_id, target_kind, display_name, subreddit, place_id,
             status, screening_verdict)
         VALUES ($1, 'community', 'r/offtopicscene', 'offtopicscene', $2,
                 'proposed', 'refused')",
    )
    .bind(ws_uuid)
    .bind(place_refused)
    .execute(&pool)
    .await?;

    let place_clean: Uuid = sqlx::query_scalar(
        "INSERT INTO discovery_places
            (workspace_id, place_kind, platform, name, url, status, membership_state)
         VALUES ($1, 'subreddit', 'reddit', 'r/realfanbase', $2, 'active', 'not_joined')
         RETURNING id",
    )
    .bind(ws_uuid)
    .bind("https://www.reddit.com/r/realfanbase")
    .fetch_one(&pool)
    .await?;

    // Unreachable agents URL: a claimed place fails the join HTTP call and
    // lands back at 'not_joined' with the error cause on membership_note.
    // A place the claim never picked keeps a NULL note — that is the
    // assertion: attempted vs. untouched.
    let executor = crowdrelay_worker::community_join_executor::CommunityJoinExecutorWorker::new(
        pool.clone(),
        ws,
        "http://127.0.0.1:1".to_string(),
        Some("test-key".to_string()),
        true,
    )?;
    executor.run_once().await?;

    let refused_note: Option<String> = sqlx::query_scalar(
        "SELECT membership_note FROM discovery_places WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws_uuid)
    .bind(place_refused)
    .fetch_one(&pool)
    .await?;
    ensure!(
        refused_note.is_none(),
        "the refused community must never reach the join call — got note {refused_note:?}"
    );

    let clean_note: Option<String> = sqlx::query_scalar(
        "SELECT membership_note FROM discovery_places WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws_uuid)
    .bind(place_clean)
    .fetch_one(&pool)
    .await?;
    ensure!(
        clean_note.is_some(),
        "control place must have been claimed and failed — note is NULL, so it was never attempted"
    );
    Ok(())
}
