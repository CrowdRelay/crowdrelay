//! Regression tests for the brain decision pipeline data-quality guards.
//!
//! NO EVIDENCE = NO OPPORTUNITY. A connector failure (Reddit credential
//! error) produces 0 confidence, 0 evidence, and "Unnamed target" — and
//! without the guards in `map_outcome`, that still became a decision with
//! an `awaiting_approval` action. These tests drive a real ingestion cycle
//! and assert the outcome is rejected (no decision, no action) for each
//! guard condition.
//!
//! They also prove the positive case: a valid outcome with evidence and a
//! real target identity flows through to a decision + action normally.

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
        .bind(format!("guards-{}", id.simple()))
        .bind("Guards Test")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(WorkspaceId::from_uuid(id))
}

/// Inserts an outcome row directly, as the agents service would.
async fn insert_outcome(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    kind: &str,
    confidence: i32,
    mut payload: serde_json::Value,
) -> Result<Uuid> {
    // Actionable kinds (require_approval) need provenance to pass the
    // admission gate. Observation kinds (recommend_only) do not.
    if matches!(
        kind,
        "outreach_targets" | "press_pitch" | "social_post" | "signal_push" | "opportunity_findings"
    ) && let Some(obj) = payload.as_object_mut()
    {
        obj.entry("provenance").or_insert(json!({
            "verification": { "status": "grounding_check_passed" },
            "context": { "any_source_failed": false, "any_source_truncated": false },
            "confidence": { "basis_points": confidence, "source": "model_self_report", "is_evidence_confidence": false },
            "model": { "actual": "test-model", "provider": "test" }
        }));
    }
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO agent_outcomes (
            id, workspace_id, task_id, result_id, kind, schema_version,
            payload, confidence_basis_points, idempotency_key, status
        ) VALUES ($1,$2,$3,$4,$5,1,$6,$7,$8,'pending')
        "#,
    )
    .bind(id)
    .bind(workspace_id.into_uuid())
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .bind(kind)
    .bind(&payload)
    .bind(confidence)
    .bind(format!("guards-test-{id}"))
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
        // Default: every channel still waits for a person, which is what
        // these guards are about.
        crowdrelay_worker::auto_post_platforms::AutoPostPlatforms::default(),
    )
}

/// Counts decisions for a workspace that came from agent outcomes.
async fn decision_count(pool: &PgPool, workspace_id: WorkspaceId) -> Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM viryaos_autopilot_decisions \
         WHERE workspace_id = $1 AND subject_kind = 'agent_outcome'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(pool)
    .await?)
}

/// Counts actions for a workspace that came from agent outcomes.
async fn action_count(pool: &PgPool, workspace_id: WorkspaceId) -> Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM viryaos_autopilot_actions \
         WHERE workspace_id = $1 AND subject_kind = 'agent_outcome'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(pool)
    .await?)
}

/// Returns the rejection_reason of the outcome, if rejected.
async fn rejection_reason(pool: &PgPool, outcome_id: Uuid) -> Result<Option<String>> {
    Ok(sqlx::query_scalar::<_, Option<String>>(
        "SELECT rejection_reason FROM agent_outcomes WHERE id = $1",
    )
    .bind(outcome_id)
    .fetch_one(pool)
    .await?)
}

/// Counts approval-requested notifications for a workspace. An
/// `awaiting_approval` action with no event here is a parked decision no
/// human ever hears about — the silence that hid four outreach targets.
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

// ── Guard: zero confidence → zero decisions ──────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn zero_confidence_outreach_target_produces_zero_decisions() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    zero_confidence_inner(&database).await
}

async fn zero_confidence_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    let outcome_id = insert_outcome(
        pool,
        ws,
        "outreach_targets",
        0,
        json!({
            "item": {
                "target_kind": "creator",
                "display_name": "r/metalpolska",
                "evidence_urls": ["https://reddit.com/r/metalpolska"],
            },
            "rationale": "test",
        }),
    )
    .await?;

    worker(pool, ws).run_once().await?;

    ensure!(
        decision_count(pool, ws).await? == 0,
        "zero-confidence outcome must not create a decision"
    );
    ensure!(
        action_count(pool, ws).await? == 0,
        "zero-confidence outcome must not create an action"
    );
    let reason = rejection_reason(pool, outcome_id).await?;
    ensure!(
        reason
            .as_ref()
            .is_some_and(|r| r.contains("INSUFFICIENT_EVIDENCE")),
        "rejection reason must mention INSUFFICIENT_EVIDENCE, got {reason:?}"
    );
    Ok(())
}

// ── Guard: zero evidence → zero decisions ────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn zero_evidence_outreach_target_produces_zero_decisions() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    zero_evidence_inner(&database).await
}

async fn zero_evidence_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    let outcome_id = insert_outcome(
        pool,
        ws,
        "outreach_targets",
        5000,
        json!({
            "item": {
                "target_kind": "creator",
                "display_name": "r/metalpolska",
                "evidence_urls": [],
            },
            "rationale": "test",
        }),
    )
    .await?;

    worker(pool, ws).run_once().await?;

    ensure!(
        decision_count(pool, ws).await? == 0,
        "zero-evidence outcome must not create a decision"
    );
    ensure!(
        action_count(pool, ws).await? == 0,
        "zero-evidence outcome must not create an action"
    );
    let reason = rejection_reason(pool, outcome_id).await?;
    ensure!(
        reason
            .as_ref()
            .is_some_and(|r| r.contains("INSUFFICIENT_EVIDENCE")),
        "rejection reason must mention INSUFFICIENT_EVIDENCE, got {reason:?}"
    );
    Ok(())
}

// ── Guard: unnamed target → zero decisions ───────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn unnamed_target_produces_zero_decisions() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    unnamed_inner(&database).await
}

async fn unnamed_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    let outcome_id = insert_outcome(
        pool,
        ws,
        "outreach_targets",
        5000,
        json!({
            "item": {
                "target_kind": "creator",
                "display_name": "Unnamed target",
                "evidence_urls": ["https://reddit.com/r/test"],
            },
            "rationale": "test",
        }),
    )
    .await?;

    worker(pool, ws).run_once().await?;

    ensure!(
        decision_count(pool, ws).await? == 0,
        "unnamed-target outcome must not create a decision"
    );
    ensure!(
        action_count(pool, ws).await? == 0,
        "unnamed-target outcome must not create an action"
    );
    let reason = rejection_reason(pool, outcome_id).await?;
    ensure!(
        reason
            .as_ref()
            .is_some_and(|r| r.contains("MISSING_TARGET_IDENTITY")),
        "rejection reason must mention MISSING_TARGET_IDENTITY, got {reason:?}"
    );
    Ok(())
}

// ── Positive: valid outcome → normal flow ────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn valid_outreach_target_produces_normal_flow() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    valid_inner(&database).await
}

async fn valid_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    insert_outcome(
        pool,
        ws,
        "outreach_targets",
        5000,
        json!({
            "item": {
                "target_kind": "creator",
                "display_name": "r/metalpolska",
                "evidence_urls": ["https://reddit.com/r/metalpolska"],
                "why_fit": "active metal community",
            },
            "rationale": "found via Reddit search",
        }),
    )
    .await?;

    let processed = worker(pool, ws).run_once().await?;
    ensure!(
        processed == 1,
        "valid outcome must be processed, got {processed}"
    );
    ensure!(
        decision_count(pool, ws).await? == 1,
        "valid outcome must create exactly one decision"
    );
    ensure!(
        action_count(pool, ws).await? == 1,
        "valid outreach target must create exactly one action"
    );
    // The action parks for a human — and the human must hear about it.
    // For hours this event never left: the action waited, nobody knew.
    ensure!(
        approval_event_count(pool, ws).await? == 1,
        "awaiting_approval action must emit exactly one approval_requested event"
    );
    // The event must name the action it asks about — an alert that cannot
    // be answered is noise.
    let event_action_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT (payload ->> 'action_id')::uuid FROM outbox_events \
         WHERE workspace_id = $1 \
           AND event_type = 'crowdrelay.autopilot.approval_requested'",
    )
    .bind(ws.into_uuid())
    .fetch_one(pool)
    .await?;
    let action_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM viryaos_autopilot_actions \
         WHERE workspace_id = $1 AND subject_kind = 'agent_outcome'",
    )
    .bind(ws.into_uuid())
    .fetch_one(pool)
    .await?;
    ensure!(
        event_action_id == action_id,
        "approval event must reference the parked action"
    );
    Ok(())
}

// ── Positive: zero-confidence insight still passes (recommend_only) ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn zero_confidence_insight_still_creates_decision() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    insight_inner(&database).await
}

async fn insight_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    insert_outcome(
        pool,
        ws,
        "generic_insight",
        0,
        json!({
            "rationale": "weak observation but still an observation",
            "kind": "generic_insight",
        }),
    )
    .await?;

    let processed = worker(pool, ws).run_once().await?;
    ensure!(
        processed == 1,
        "zero-confidence insight must still be processed, got {processed}"
    );
    ensure!(
        decision_count(pool, ws).await? == 1,
        "zero-confidence insight must create a decision (it's an observation, not an action)"
    );
    // Insights are recommend_only — no action.
    ensure!(
        action_count(pool, ws).await? == 0,
        "insight must not create an action (recommend_only)"
    );
    Ok(())
}

// ── The grounding gate: what shut production for nine days ───────────
//
// `provenance_admission` refuses a `require_approval` outcome whose
// verification status is not `grounding_check_passed`. That is correct and
// deliberately fail-closed — an outcome nobody checked must not become an
// action that reaches fans, journalists or communities.
//
// Nothing tested it. Every verifier in the agent service returned HTTP 429
// against an exhausted free-tier daily quota, so every outcome arrived
// `not_verified`, and every actionable one was refused. No post was drafted,
// no artifact published, and — because `dispatch_reached_an_audience` will not
// resolve a dispatch that reached nobody — 57 evidence rows stayed unresolved.
// Zero learning for nine days, and no test, gauge or alarm said so.
//
// The three tests below pin the gate from both sides and pin the asymmetry
// that made the outage invisible.

fn unverified_provenance(confidence: i32) -> serde_json::Value {
    json!({
        "verification": { "status": "not_verified" },
        "context": { "any_source_failed": false, "any_source_truncated": false },
        "confidence": { "basis_points": confidence, "source": "model_self_report", "is_evidence_confidence": false },
        "model": { "actual": "test-model", "provider": "test" }
    })
}

fn outreach_item() -> serde_json::Value {
    json!({
        "target_kind": "creator",
        "display_name": "r/metalpolska",
        "evidence_urls": ["https://reddit.com/r/metalpolska"],
        "why_fit": "active metal community",
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn unverified_actionable_outcome_creates_nothing() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    unverified_actionable_inner(&database).await
}

async fn unverified_actionable_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    let id = insert_outcome(
        pool,
        ws,
        "outreach_targets",
        5000,
        json!({
            "item": outreach_item(),
            "rationale": "found via Reddit search",
            "provenance": unverified_provenance(5000),
        }),
    )
    .await?;

    worker(pool, ws).run_once().await?;
    ensure!(
        decision_count(pool, ws).await? == 0,
        "an unverified actionable outcome must not create a decision"
    );
    ensure!(
        action_count(pool, ws).await? == 0,
        "an unverified actionable outcome must not create an action"
    );
    // The reason has to name the gate. This string is what an operator greps
    // for, and it is the only durable record of why the loop is not moving.
    let reason = rejection_reason(pool, id).await?.unwrap_or_default();
    ensure!(
        reason.contains("NOT_GROUNDING_CHECKED"),
        "the refusal must name the grounding gate, got {reason:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_passing_grounding_check_opens_the_same_gate() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    passing_grounding_inner(&database).await
}

/// The other half of the pair. Without this, a gate that refused *everything*
/// would still pass the test above, which is exactly the failure mode being
/// guarded against: production refused every outcome and looked healthy.
async fn passing_grounding_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    insert_outcome(
        pool,
        ws,
        "outreach_targets",
        5000,
        json!({
            "item": outreach_item(),
            "rationale": "found via Reddit search",
            "provenance": {
                "verification": { "status": "grounding_check_passed" },
                "context": { "any_source_failed": false, "any_source_truncated": false },
                "confidence": { "basis_points": 5000, "source": "model_self_report", "is_evidence_confidence": false },
                "model": { "actual": "test-model", "provider": "test" }
            },
        }),
    )
    .await?;

    worker(pool, ws).run_once().await?;
    ensure!(
        decision_count(pool, ws).await? == 1,
        "a grounding-checked outcome must create exactly one decision"
    );
    ensure!(
        action_count(pool, ws).await? == 1,
        "a grounding-checked outcome must create exactly one action"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_unverified_observation_still_reaches_the_board() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    unverified_observation_inner(&database).await
}

/// The asymmetry that hid the outage, pinned deliberately.
///
/// The gate applies to `require_approval` kinds only. `recommend_only` kinds
/// pass through unverified, because gating an observation would delete
/// intelligence instead of labelling it. That is the right call and it has a
/// consequence worth stating: while every actionable outcome was being
/// refused, insights and segments kept flowing, so the board filled up and the
/// system looked busy. Anyone watching the board saw a working brain.
///
/// This test exists so that behaviour stays intentional rather than becoming
/// a thing someone "fixes" by gating observations too, or by ungating actions.
async fn unverified_observation_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    insert_outcome(
        pool,
        ws,
        "campaign_insight",
        0,
        json!({
            "item": { "headline": "engagement is up on Thursdays" },
            "rationale": "observed in campaign stats",
            "provenance": unverified_provenance(0),
        }),
    )
    .await?;

    worker(pool, ws).run_once().await?;
    ensure!(
        decision_count(pool, ws).await? == 1,
        "an observation must reach the board even unverified"
    );
    ensure!(
        action_count(pool, ws).await? == 0,
        "an observation must never create an action"
    );
    Ok(())
}

// ── Regression: a missing rationale must not die on reason_check ──────
//
// `payload.rationale` deserializes with `#[serde(default)]` — an outcome
// that carries no rationale produces `""`, and `viryaos_autopilot_decisions
// .reason` is CHECKed `btrim <> ''`. One production outcome (2026-08-28,
// press_pitch) hit exactly this: the decision INSERT violated the CHECK,
// the whole transaction aborted, and the outcome was rejected with a
// database error as its only record.
//
// The fix states the absence instead of discarding the work.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_outcome_without_a_rationale_still_maps() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    rationaleless_inner(&database).await
}

async fn rationaleless_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    let id = insert_outcome(
        pool,
        ws,
        "campaign_insight",
        0,
        // No `rationale` key at all — `#[serde(default)]` yields "".
        json!({
            "item": { "headline": "engagement is up on Thursdays" },
        }),
    )
    .await?;

    worker(pool, ws).run_once().await?;
    ensure!(
        decision_count(pool, ws).await? == 1,
        "a rationale-less outcome must still map to a decision"
    );
    let reason: String = sqlx::query_scalar(
        "SELECT reason FROM viryaos_autopilot_decisions \
         WHERE workspace_id = $1 AND subject_kind = 'agent_outcome'",
    )
    .bind(ws.into_uuid())
    .fetch_one(pool)
    .await?;
    ensure!(
        reason == "Outcome supplied no rationale.",
        "the decision reason must state the absence, got {reason:?}"
    );
    let status: String = sqlx::query_scalar("SELECT status FROM agent_outcomes WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await?;
    ensure!(
        status == "processed",
        "the outcome must be processed, not rejected — got {status}"
    );
    Ok(())
}

// ── Scout findings: link-or-drop, vocabulary, and the review surface ───────
//
// An `opportunity_findings` outcome is a scout carrying a link home. The
// worker turns it into a `viryaos_team_opportunities` row plus a decision
// that names the row — never an action, because the review act lives on the
// shortlist's own controls. A finding without a usable link, a readable
// description, or a kind inside the scout vocabulary is not a finding.

/// A minimal valid finding item. `press` exercises a scout-only kind so the
/// row proves scout vocabulary reaches the table.
fn finding_item() -> serde_json::Value {
    json!({
        "type": "opportunity_finding",
        "opportunity_kind": "press",
        "title": "Unsigned column at Obscure Zine",
        "organization": "Obscure Zine",
        "destination_url": "https://obscure.example/columns/unsigned",
        "summary": "A quarterly print zine with an unsigned column and an email pitch box.",
        "country_code": "PL",
    })
}

async fn opportunity_count(pool: &PgPool, workspace_id: WorkspaceId) -> Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM viryaos_team_opportunities WHERE workspace_id = $1",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(pool)
    .await?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_finding_without_a_link_is_rejected() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    finding_no_link_inner(&database).await
}

async fn finding_no_link_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    let mut item = finding_item();
    item.as_object_mut().unwrap().remove("destination_url");
    let outcome_id = insert_outcome(
        pool,
        ws,
        "opportunity_findings",
        5000,
        json!({ "item": item, "rationale": "test" }),
    )
    .await?;

    worker(pool, ws).run_once().await?;

    ensure!(
        opportunity_count(pool, ws).await? == 0,
        "a finding without a link must not land a row"
    );
    let reason = rejection_reason(pool, outcome_id).await?;
    ensure!(
        reason
            .as_ref()
            .is_some_and(|r| r.contains("MISSING_FINDING_LINK")),
        "rejection reason must mention MISSING_FINDING_LINK, got {reason:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_finding_outside_the_vocabulary_is_rejected() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    finding_bad_kind_inner(&database).await
}

async fn finding_bad_kind_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    let mut item = finding_item();
    item["opportunity_kind"] = json!("funding");
    let outcome_id = insert_outcome(
        pool,
        ws,
        "opportunity_findings",
        5000,
        json!({ "item": item, "rationale": "test" }),
    )
    .await?;

    worker(pool, ws).run_once().await?;

    ensure!(
        opportunity_count(pool, ws).await? == 0,
        "a kind outside the scout vocabulary must not land a row"
    );
    let reason = rejection_reason(pool, outcome_id).await?;
    ensure!(
        reason
            .as_ref()
            .is_some_and(|r| r.contains("INVALID_FINDING_KIND")),
        "rejection reason must mention INVALID_FINDING_KIND, got {reason:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_valid_finding_lands_a_row_and_a_decision_but_no_action() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    finding_valid_inner(&database).await
}

async fn finding_valid_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    let outcome_id = insert_outcome(
        pool,
        ws,
        "opportunity_findings",
        5000,
        json!({ "item": finding_item(), "rationale": "test" }),
    )
    .await?;

    worker(pool, ws).run_once().await?;

    ensure!(
        opportunity_count(pool, ws).await? == 1,
        "a valid finding must land exactly one opportunity row"
    );
    let (kind, verified, requires_contract, observed, status): (String, bool, bool, bool, String) =
        sqlx::query_as(
            "SELECT opportunity_kind, verified_destination, requires_contract, \
                source_observed_at IS NOT NULL, status \
         FROM viryaos_team_opportunities WHERE workspace_id = $1",
        )
        .bind(ws.into_uuid())
        .fetch_one(pool)
        .await?;
    ensure!(kind == "press", "the scout kind must land, got {kind}");
    ensure!(
        !verified && requires_contract && status == "new",
        "a finding must land unverified, contract-gated and new"
    );
    ensure!(
        observed,
        "a finding without observed_at borrows the outcome's write time"
    );

    // The decision names the opportunity row — the shortlist joins on it.
    let (subject_kind, subject_matches): (String, bool) = sqlx::query_as(
        "SELECT d.subject_kind, d.subject_id = o.id \
         FROM viryaos_autopilot_decisions d \
         JOIN viryaos_team_opportunities o ON o.workspace_id = d.workspace_id \
         WHERE d.workspace_id = $1",
    )
    .bind(ws.into_uuid())
    .fetch_one(pool)
    .await?;
    ensure!(
        subject_kind == "team_opportunity" && subject_matches,
        "the decision must name the opportunity row it is about"
    );

    // No action: the review act lives on the shortlist's own controls, and an
    // approved action no executor claims would sit queued forever.
    let actions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM viryaos_autopilot_actions WHERE workspace_id = $1",
    )
    .bind(ws.into_uuid())
    .fetch_one(pool)
    .await?;
    ensure!(actions == 0, "a finding must create no action row");

    let status: String = sqlx::query_scalar("SELECT status FROM agent_outcomes WHERE id = $1")
        .bind(outcome_id)
        .fetch_one(pool)
        .await?;
    ensure!(status == "processed", "the outcome must be processed");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_reemitted_finding_updates_the_same_row() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    finding_reemit_inner(&database).await
}

async fn finding_reemit_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    for _ in 0..2 {
        insert_outcome(
            pool,
            ws,
            "opportunity_findings",
            5000,
            json!({ "item": finding_item(), "rationale": "test" }),
        )
        .await?;
    }

    worker(pool, ws).run_once().await?;

    ensure!(
        opportunity_count(pool, ws).await? == 1,
        "kind + link is the finding's natural key — a re-emit must land on the same row"
    );
    let version: i64 = sqlx::query_scalar(
        "SELECT version FROM viryaos_team_opportunities WHERE workspace_id = $1",
    )
    .bind(ws.into_uuid())
    .fetch_one(pool)
    .await?;
    ensure!(
        version == 2,
        "the re-emit must bump the row version, got {version}"
    );
    Ok(())
}

// ── Regression: one community is one row ─────────────────────────────
//
// The table's only uniqueness was (workspace_id, display_name,
// target_kind), but a community's identity is its subreddit — the scanner
// named the same sub "r/deathcore" one week and "/r/Deathcore - news,
// reviews & discussion" the next, and both rows promoted. Production held
// eleven doubled subreddits, every one drafted twice per wave: the relay
// saw two admitted targets and the board saw two approvals for one post.
//
// The subreddit-arbiter upsert conflicts on normalize_subreddit(subreddit)
// — lowercase, leading r/ or /r/ stripped — so the second proposal lands
// on the first row. The pair below is the production shape: bare name
// first, decorated display name and prefixed subreddit second.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_reproposed_subreddit_lands_on_the_same_row() -> Result<()> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    community_dedup_inner(&database).await
}

async fn community_dedup_inner(pool: &PgPool) -> Result<()> {
    let ws = workspace(pool).await?;
    for (display_name, subreddit) in [
        ("r/deathcore", "deathcore"),
        ("/r/Deathcore - news, reviews & discussion", "/r/Deathcore"),
    ] {
        insert_outcome(
            pool,
            ws,
            "outreach_targets",
            5000,
            json!({
                "item": {
                    "target_kind": "community",
                    "display_name": display_name,
                    "subreddit": subreddit,
                    "evidence_urls": ["https://reddit.com/r/deathcore"],
                    "why_fit": "active death metal community",
                },
                "rationale": "scanner proposal",
            }),
        )
        .await?;
    }

    worker(pool, ws).run_once().await?;

    let rows: Vec<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT display_name, subreddit, status FROM agent_outreach_targets \
         WHERE workspace_id = $1 AND target_kind = 'community'",
    )
    .bind(ws.into_uuid())
    .fetch_all(pool)
    .await?;
    ensure!(
        rows.len() == 1,
        "two proposals for one subreddit must produce one target row, got {rows:?}"
    );
    let (display_name, subreddit, _) = &rows[0];
    ensure!(
        subreddit.as_deref() == Some("deathcore"),
        "the stored subreddit is the canonical identity, got {subreddit:?}"
    );
    // The first display name survives — it is the label screening recorded
    // against, and refreshing it to a later scan's phrasing churns the card
    // an operator learned to recognize.
    ensure!(
        display_name == "r/deathcore",
        "a re-proposal must not rename the row, got {display_name:?}"
    );
    Ok(())
}
