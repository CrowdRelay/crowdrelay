//! Event-network scout outcome -> beacon candidate, end to end.
//!
//! The scout template discovers local partners around a pinned event. The
//! worker admits a candidate only when the model cited URLs and contacts
//! that the task's recorded evidence actually showed it — the original brief
//! contains no URLs, because discovery happens inside the run's context, so
//! prompt-text matching rejected every honest finding (the bug this sprint
//! closed).
//!
//! These tests drive the real ingestion cycle (`AgentOutcomeWorker::run_once`)
//! against a disposable database: a task row with evidence metadata, an
//! outcome row as the agents service would write it, then the assertions an
//! operator cares about:
//!
//! * a grounded candidate lands `verified=false, accepts_outreach=false` —
//!   reviewable, never contactable on the model's say-so;
//! * an invented URL, a foreign event or a non-scout template is rejected;
//! * a repeat match reuses the beacon row, appends a `beacon_event_matches`
//!   row, and never rolls back an operator's verification state.

use crate::common;

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_worker::agent_outcomes::AgentOutcomeWorker;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

const SCOUT_TEMPLATE: &str = "event-network-scout";

async fn workspace(pool: &PgPool) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("scout-{}", id.simple()))
        .bind("Scout Test")
        .execute(pool)
        .await
        .context("insert workspace")?;
    Ok(WorkspaceId::from_uuid(id))
}

/// `agent_service_tasks` belongs to the agents service — no CrowdRelay
/// migration creates it, so the suite database needs the columns the
/// producing-task join reads.
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

async fn city(pool: &PgPool) -> Result<Uuid> {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ($1, 'Scout City', 'PL', 52.23, 21.01)
         RETURNING id",
    )
    .bind(format!("scout-city-{}", Uuid::now_v7().simple()))
    .fetch_one(pool)
    .await
    .context("insert city")
}

async fn event(pool: &PgPool, workspace_id: WorkspaceId, city_id: Uuid) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, city_id, slug, title, starts_at, status, published_at)
         VALUES ($1, $2, $3, $4, 'A show', now() + interval '30 days', 'published', now())",
    )
    .bind(id)
    .bind(workspace_id.into_uuid())
    .bind(city_id)
    .bind(format!("scout-show-{}", id.simple()))
    .execute(pool)
    .await
    .context("insert event")?;
    Ok(id)
}

/// The producing scout task. `evidence` is what the context builder actually
/// rendered into the model's prompt; `subject_event_id` is the event the
/// dispatch pinned. Both live in metadata — the prompt itself carries
/// neither.
async fn scout_task(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    template: &str,
    subject_event_id: Uuid,
    evidence: serde_json::Value,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO agent_service_tasks
            (id, workspace_id, template_id, model_id, prompt, status, tier, metadata)
        VALUES ($1,$2,$3,'auto','scout brief','completed','basic',$4)
        "#,
    )
    .bind(id)
    .bind(workspace_id.into_uuid())
    .bind(template)
    .bind(json!({
        "subject_event_id": subject_event_id,
        "evidence": evidence,
    }))
    .execute(pool)
    .await
    .context("insert scout task")?;
    Ok(id)
}

/// An outcome row as `emitOutcomes` writes it: one row per item, provenance
/// block required for every `require_approval` kind.
async fn insert_outcome(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    task_id: Uuid,
    item: serde_json::Value,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO agent_outcomes (
            id, workspace_id, task_id, result_id, kind, schema_version,
            payload, confidence_basis_points, idempotency_key, status
        ) VALUES ($1,$2,$3,$4,'beacon_candidates',1,$5,8000,$6,'pending')
        "#,
    )
    .bind(id)
    .bind(workspace_id.into_uuid())
    .bind(task_id)
    .bind(Uuid::now_v7())
    .bind(json!({
        "item": item,
        "rationale": "local partner candidates for the pinned show",
        "provenance": {
            "verification": { "status": "grounding_check_passed" },
            "context": { "any_source_failed": false, "any_source_truncated": false },
            "confidence": {
                "basis_points": 8000,
                "source": "model_self_report",
                "is_evidence_confidence": false
            },
            "model": { "actual": "test-model", "provider": "test" }
        }
    }))
    .bind(format!("scout-test-{id}"))
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

async fn outcome_status(pool: &PgPool, outcome_id: Uuid) -> Result<(String, Option<String>)> {
    Ok(sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT status, rejection_reason FROM agent_outcomes WHERE id = $1",
    )
    .bind(outcome_id)
    .fetch_one(pool)
    .await?)
}

async fn beacon_rows(
    pool: &PgPool,
    workspace_id: WorkspaceId,
) -> Result<Vec<(Uuid, bool, bool, bool, serde_json::Value)>> {
    Ok(
        sqlx::query_as::<_, (Uuid, bool, bool, bool, serde_json::Value)>(
            "SELECT id, verified, accepts_outreach, do_not_contact, metadata
         FROM beacons WHERE workspace_id = $1 ORDER BY created_at",
        )
        .bind(workspace_id.into_uuid())
        .fetch_all(pool)
        .await?,
    )
}

async fn match_rows(pool: &PgPool, workspace_id: WorkspaceId) -> Result<Vec<(Uuid, Uuid, Uuid)>> {
    Ok(sqlx::query_as::<_, (Uuid, Uuid, Uuid)>(
        "SELECT beacon_id, event_id, outcome_id
         FROM beacon_event_matches WHERE workspace_id = $1 ORDER BY matched_at",
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await?)
}

/// The evidence bundle a completed scout task carries: every URL the model
/// saw, each with the snippet and tool that fetched it, plus the contacts
/// the rendered context surfaced.
fn evidence() -> serde_json::Value {
    json!({
        "version": 1,
        "urls": [
            {
                "url": "https://radio-ostrow.pl/kontakt",
                "tool": "web_search",
                "snippet": "Radio Ostrow lokalna rozgłośnia — kontakt z redakcją.",
                "fetched_at": "2026-10-01T12:00:00Z"
            },
            {
                "url": "https://ck-ostrow.pl/wydarzenia",
                "tool": "web_search",
                "snippet": "Centrum kultury — scena klubowa i współprace.",
                "fetched_at": "2026-10-01T12:00:00Z"
            }
        ],
        "contacts": [
            { "kind": "email", "value": "redakcja@radio-ostrow.pl" }
        ]
    })
}

// ── Admit: grounded candidate lands unverified, with match history ────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn grounded_scout_candidate_lands_as_unverified_beacon() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    create_foreign_task_table(&pool).await?;

    let ws = workspace(&pool).await?;
    let city_id = city(&pool).await?;
    let event_id = event(&pool, ws, city_id).await?;
    let task_id = scout_task(&pool, ws, SCOUT_TEMPLATE, event_id, evidence()).await?;
    let outcome_id = insert_outcome(
        &pool,
        ws,
        task_id,
        json!({
            "event_id": event_id,
            "beacon_kind": "radio",
            "display_name": "Radio Ostrów",
            "contact_email": "redakcja@radio-ostrow.pl",
            "source_url": "https://radio-ostrow.pl/kontakt",
            "why_fit": "Local radio covering the show's city"
        }),
    )
    .await?;

    worker(&pool, ws).run_once().await?;

    let (status, _) = outcome_status(&pool, outcome_id).await?;
    ensure!(
        status == "processed",
        "outcome should be processed, got {status}"
    );

    let beacons = beacon_rows(&pool, ws).await?;
    ensure!(
        beacons.len() == 1,
        "expected one beacon, got {}",
        beacons.len()
    );
    let (beacon_id, verified, accepts_outreach, do_not_contact, metadata) = &beacons[0];
    ensure!(
        !verified && !accepts_outreach && !do_not_contact,
        "a scout candidate must land unverified and non-contactable"
    );
    let scout = &metadata["event_network_scout"];
    ensure!(
        scout["why_fit"].as_str() == Some("Local radio covering the show's city"),
        "the review surface needs the model's fit reason, got {scout}"
    );
    ensure!(
        scout["evidence"]["snippet"].as_str().is_some(),
        "the beacon row must carry the proof the model saw, got {scout}"
    );

    let matches_ = match_rows(&pool, ws).await?;
    ensure!(
        matches_.len() == 1,
        "expected one match row, got {}",
        matches_.len()
    );
    ensure!(
        matches_[0] == (*beacon_id, event_id, outcome_id),
        "the match must record beacon, event and producing outcome"
    );

    let decision_kind: Option<(String, Uuid)> = sqlx::query_as(
        "SELECT subject_kind, subject_id FROM autopilot_decisions \
         WHERE workspace_id = $1 AND subject_kind = 'beacon'",
    )
    .bind(ws.into_uuid())
    .fetch_optional(&pool)
    .await?;
    ensure!(
        decision_kind.map(|(_, id)| id) == Some(*beacon_id),
        "the decision must point at the beacon it admitted"
    );
    Ok(())
}

// ── Reject: a known band is not a generic creator ─────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn known_peer_band_cannot_enter_creator_short_form_lane() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    create_foreign_task_table(&pool).await?;

    let ws = workspace(&pool).await?;
    let city_id = city(&pool).await?;
    let event_id = event(&pool, ws, city_id).await?;

    // The shared act registry already knows this is a band. A model seeing
    // its YouTube page must not reinterpret "has a channel" as "is a creator"
    // and thereby unlock co_post / short_form_clip.
    let peer_name = format!("Misscore {}", Uuid::now_v7().simple());
    sqlx::query(
        "INSERT INTO place_peer_acts (name_key, display_name)
         VALUES (place_venue_key($1), $1)",
    )
    .bind(&peer_name)
    .execute(&pool)
    .await?;

    let peer_evidence = json!({
        "version": 1,
        "urls": [{
            "url": "https://radio-ostrow.pl/kontakt",
            "tool": "web_search",
            "snippet": "Public page for a known peer act.",
            "fetched_at": "2026-10-01T12:00:00Z"
        }],
        "contacts": []
    });
    let task_id = scout_task(&pool, ws, SCOUT_TEMPLATE, event_id, peer_evidence).await?;
    let outcome_id = insert_outcome(
        &pool,
        ws,
        task_id,
        json!({
            "event_id": event_id,
            "beacon_kind": "creator",
            "display_name": peer_name,
            "source_url": "https://radio-ostrow.pl/kontakt",
            "why_fit": "has a YouTube channel"
        }),
    )
    .await?;

    worker(&pool, ws).run_once().await?;

    let (status, reason) = outcome_status(&pool, outcome_id).await?;
    ensure!(
        status == "rejected",
        "a known band must not enter the generic creator lane, got {status}"
    );
    ensure!(
        reason
            .as_deref()
            .unwrap_or_default()
            .contains("known peer act"),
        "rejection must explain the role collision, got {reason:?}"
    );
    ensure!(
        beacon_rows(&pool, ws).await?.is_empty(),
        "rejected peer-as-creator must write no beacon"
    );
    Ok(())
}

// ── Reject: URLs the model was never shown ────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn candidate_url_absent_from_evidence_is_rejected() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    create_foreign_task_table(&pool).await?;

    let ws = workspace(&pool).await?;
    let city_id = city(&pool).await?;
    let event_id = event(&pool, ws, city_id).await?;
    let task_id = scout_task(&pool, ws, SCOUT_TEMPLATE, event_id, evidence()).await?;
    // The invented URL: nowhere in the task's recorded evidence.
    let outcome_id = insert_outcome(
        &pool,
        ws,
        task_id,
        json!({
            "event_id": event_id,
            "beacon_kind": "promoter",
            "display_name": "Invented Promoter",
            "source_url": "https://invented-promoter.example/booking",
            "why_fit": "plausible sounding"
        }),
    )
    .await?;

    worker(&pool, ws).run_once().await?;

    let (status, reason) = outcome_status(&pool, outcome_id).await?;
    ensure!(
        status == "rejected",
        "ungrounded URL must reject, got {status}"
    );
    ensure!(
        reason.as_deref().unwrap_or_default().contains("evidence"),
        "rejection should name the missing evidence, got {reason:?}"
    );
    ensure!(
        beacon_rows(&pool, ws).await?.is_empty(),
        "no beacon may be written"
    );
    ensure!(
        match_rows(&pool, ws).await?.is_empty(),
        "no match may be written"
    );
    Ok(())
}

// ── Reject: a different show's finding ────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn candidate_for_another_event_is_rejected() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    create_foreign_task_table(&pool).await?;

    let ws = workspace(&pool).await?;
    let city_id = city(&pool).await?;
    let pinned = event(&pool, ws, city_id).await?;
    let other = event(&pool, ws, city_id).await?;
    let task_id = scout_task(&pool, ws, SCOUT_TEMPLATE, pinned, evidence()).await?;
    // The model claims the candidate is for a different event than the task
    // was pinned to — cross-show research is not this task's output.
    let outcome_id = insert_outcome(
        &pool,
        ws,
        task_id,
        json!({
            "event_id": other,
            "beacon_kind": "venue",
            "display_name": "Klub Pod Minogą",
            "source_url": "https://radio-ostrow.pl/kontakt",
            "why_fit": "wrong show"
        }),
    )
    .await?;

    worker(&pool, ws).run_once().await?;

    let (status, reason) = outcome_status(&pool, outcome_id).await?;
    ensure!(
        status == "rejected",
        "foreign event must reject, got {status}"
    );
    ensure!(
        reason.as_deref().unwrap_or_default().contains("pinned"),
        "rejection should name the event pinning, got {reason:?}"
    );
    ensure!(
        beacon_rows(&pool, ws).await?.is_empty(),
        "no beacon may be written"
    );
    Ok(())
}

// ── Reject: evidence on another task authorizes nothing ───────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn evidence_on_a_different_task_authorizes_nothing() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    create_foreign_task_table(&pool).await?;

    let ws = workspace(&pool).await?;
    let city_id = city(&pool).await?;
    let event_id = event(&pool, ws, city_id).await?;
    // Task A holds the evidence; the outcome claims task B — a task without
    // evidence cannot ground the same URL.
    let _task_with_evidence = scout_task(&pool, ws, SCOUT_TEMPLATE, event_id, evidence()).await?;
    let task_without = scout_task(
        &pool,
        ws,
        SCOUT_TEMPLATE,
        event_id,
        json!({
            "version": 1, "urls": [], "contacts": []
        }),
    )
    .await?;
    let outcome_id = insert_outcome(
        &pool,
        ws,
        task_without,
        json!({
            "event_id": event_id,
            "beacon_kind": "radio",
            "display_name": "Radio Ostrów",
            "contact_email": "redakcja@radio-ostrow.pl",
            "source_url": "https://radio-ostrow.pl/kontakt",
            "why_fit": "borrowed evidence"
        }),
    )
    .await?;

    worker(&pool, ws).run_once().await?;

    let (status, _) = outcome_status(&pool, outcome_id).await?;
    ensure!(
        status == "rejected",
        "evidence is task-scoped — another task's URLs cannot ground this outcome"
    );
    ensure!(
        beacon_rows(&pool, ws).await?.is_empty(),
        "no beacon may be written"
    );
    Ok(())
}

// ── Reject: only the scout template may emit beacon_candidates ────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn candidate_from_a_non_scout_template_is_rejected() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    create_foreign_task_table(&pool).await?;

    let ws = workspace(&pool).await?;
    let city_id = city(&pool).await?;
    let event_id = event(&pool, ws, city_id).await?;
    let task_id = scout_task(&pool, ws, "community-engager", event_id, evidence()).await?;
    let outcome_id = insert_outcome(
        &pool,
        ws,
        task_id,
        json!({
            "event_id": event_id,
            "beacon_kind": "radio",
            "display_name": "Radio Ostrów",
            "contact_email": "redakcja@radio-ostrow.pl",
            "source_url": "https://radio-ostrow.pl/kontakt",
            "why_fit": "template spoof"
        }),
    )
    .await?;

    worker(&pool, ws).run_once().await?;

    let (status, reason) = outcome_status(&pool, outcome_id).await?;
    ensure!(
        status == "rejected",
        "non-scout template must reject, got {status}"
    );
    ensure!(
        reason
            .as_deref()
            .unwrap_or_default()
            .contains("event-network-scout"),
        "rejection should name the allowed template, got {reason:?}"
    );
    Ok(())
}

// ── Repeat match: dedup keeps the row and the operator's flags ────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn repeat_match_reuses_beacon_and_preserves_operator_state() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    create_foreign_task_table(&pool).await?;

    let ws = workspace(&pool).await?;
    let city_id = city(&pool).await?;

    // First show: the radio station is found and admitted.
    let event_a = event(&pool, ws, city_id).await?;
    let task_a = scout_task(&pool, ws, SCOUT_TEMPLATE, event_a, evidence()).await?;
    let outcome_a = insert_outcome(
        &pool,
        ws,
        task_a,
        json!({
            "event_id": event_a,
            "beacon_kind": "radio",
            "display_name": "Radio Ostrów",
            "contact_email": "redakcja@radio-ostrow.pl",
            "source_url": "https://radio-ostrow.pl/kontakt",
            "why_fit": "first show"
        }),
    )
    .await?;
    worker(&pool, ws).run_once().await?;
    let beacons = beacon_rows(&pool, ws).await?;
    ensure!(beacons.len() == 1);
    let beacon_id = beacons[0].0;

    // The operator has meanwhile reviewed the row: verified, but flagged
    // do-not-contact for this channel.
    sqlx::query("UPDATE beacons SET verified = true, do_not_contact = true WHERE id = $1")
        .bind(beacon_id)
        .execute(&pool)
        .await?;

    // Second show in the same city: the scout finds the same station again.
    let event_b = event(&pool, ws, city_id).await?;
    let task_b = scout_task(&pool, ws, SCOUT_TEMPLATE, event_b, evidence()).await?;
    let outcome_b = insert_outcome(
        &pool,
        ws,
        task_b,
        json!({
            "event_id": event_b,
            "beacon_kind": "radio",
            "display_name": "Radio Ostrów",
            "contact_email": "redakcja@radio-ostrow.pl",
            "source_url": "https://radio-ostrow.pl/kontakt",
            "why_fit": "second show, same partner"
        }),
    )
    .await?;
    worker(&pool, ws).run_once().await?;

    let (status_b, _) = outcome_status(&pool, outcome_b).await?;
    ensure!(
        status_b == "processed",
        "repeat match should still process, got {status_b}"
    );

    // Identity deduped on (workspace, city, kind, email): still one row.
    let beacons = beacon_rows(&pool, ws).await?;
    ensure!(
        beacons.len() == 1 && beacons[0].0 == beacon_id,
        "a repeat match must reuse the canonical beacon row"
    );
    let (_, verified, _, do_not_contact, metadata) = &beacons[0];
    ensure!(
        *verified && *do_not_contact,
        "operator state must survive a repeat match: verified={verified}, do_not_contact={do_not_contact}"
    );

    // Match history accumulated — one row per (beacon, event).
    let matches_ = match_rows(&pool, ws).await?;
    ensure!(
        matches_.len() == 2,
        "expected two match rows, got {}",
        matches_.len()
    );
    ensure!(
        matches_.iter().all(|(b, _, _)| *b == beacon_id),
        "every match must point at the same beacon"
    );
    ensure!(
        matches_
            .iter()
            .any(|(_, e, o)| *e == event_a && *o == outcome_a)
            && matches_
                .iter()
                .any(|(_, e, o)| *e == event_b && *o == outcome_b),
        "each match records its own event and outcome"
    );
    // Latest scout object carries the second match; earlier history lives in
    // the matches table, so the merge is additive rather than a replace.
    ensure!(
        metadata["event_network_scout"]["agent_outcome_id"].as_str()
            == Some(&outcome_b.to_string()),
        "the scout block should track the latest admission"
    );
    Ok(())
}
