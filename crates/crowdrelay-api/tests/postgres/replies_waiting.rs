//! "Answered you, your turn": the reply triage view reads the interaction
//! ledger, not only the classifier's table.
//!
//! On 2026-09-25 production held sixteen contacts whose last message was
//! theirs, two of them positive, while the triage view answered "nothing
//! needs you": every one of those replies came in through the reply route or
//! a sheet import, and neither passes the classifier. What only a routed
//! request against a real schema proves: which contacts count as waiting (the
//! latest message is theirs, recent, not a refusal, the contact still
//! writable), the order (positive first, then the longest wait), the label
//! the source gave the answer, and that another workspace's contacts stay
//! out.
//!
//! Runs under `just test-postgres` against `CROWDRELAY_TEST_DATABASE_URL`.

use crate::{attestation_anchor, common};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header::AUTHORIZATION},
};
use crowdrelay_domain::WorkspaceId;
use serde_json::{Value, json};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;
use uuid::Uuid;

const CONTROL_PLANE_KEY: &str = "test-control-plane-key-123456789012";

async fn target(
    pool: &PgPool,
    workspace: Uuid,
    name: &str,
    do_not_contact: bool,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO outreach_targets
             (id, workspace_id, target_kind, display_name, contact_email, do_not_contact)
         VALUES ($1, $2, 'press', $3, $4, $5)",
    )
    .bind(id)
    .bind(workspace)
    .bind(name)
    .bind(format!(
        "{}@test.example",
        name.to_lowercase().replace(' ', "-")
    ))
    .bind(do_not_contact)
    .execute(pool)
    .await?;
    Ok(id)
}

async fn message(
    pool: &PgPool,
    workspace: Uuid,
    target: Uuid,
    inbound: bool,
    disposition: &str,
    at: OffsetDateTime,
    metadata: Value,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO outreach_interactions
             (workspace_id, target_id, direction, phase, disposition, source_key, occurred_at, metadata)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(workspace)
    .bind(target)
    .bind(if inbound { "inbound" } else { "outbound" })
    .bind(if inbound { "reply" } else { "initial" })
    .bind(disposition)
    .bind(format!("test:{}", Uuid::now_v7()))
    .bind(at)
    .bind(metadata)
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn contacts_whose_last_word_is_theirs_are_waiting_on_the_act()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let ws = attestation_anchor::seed_workspace(&pool).await?;
    let other = attestation_anchor::seed_workspace(&pool).await?;
    let app = crowdrelay_api::router(
        attestation_anchor::app_state(&pool, WorkspaceId::from_uuid(ws))?,
        crowdrelay_api::HttpConfig::new(["http://localhost:4321".to_owned()])?,
    );
    let now = OffsetDateTime::now_utc();
    let days = |n: i64| now - Duration::days(n);

    // Waiting, received: wrote 10 days ago, they answered 9 days ago.
    let zine = target(&pool, ws, "Old Zine", false).await?;
    message(&pool, ws, zine, false, "none", days(10), json!({})).await?;
    message(
        &pool,
        ws,
        zine,
        true,
        "received",
        days(9),
        json!({"result": "Decision required"}),
    )
    .await?;
    // Waiting, positive, more recent: still listed first.
    let radio = target(&pool, ws, "Night Radio", false).await?;
    message(&pool, ws, radio, false, "none", days(3), json!({})).await?;
    message(
        &pool,
        ws,
        radio,
        true,
        "positive",
        days(2),
        json!({"response_type": "POSITIVE"}),
    )
    .await?;
    // Answered, then the act wrote back: not waiting.
    let blog = target(&pool, ws, "Answered Blog", false).await?;
    message(&pool, ws, blog, true, "positive", days(5), json!({})).await?;
    message(&pool, ws, blog, false, "none", days(4), json!({})).await?;
    // A refusal asks nothing of the act.
    let fest = target(&pool, ws, "Declining Fest", false).await?;
    message(&pool, ws, fest, true, "declined", days(1), json!({})).await?;
    // A contact that must not be written to is not "your turn".
    let dnc = target(&pool, ws, "Do Not Write", true).await?;
    message(&pool, ws, dnc, true, "received", days(1), json!({})).await?;
    // An answer older than the window is a lapsed conversation.
    let stale = target(&pool, ws, "Stale Venue", false).await?;
    message(&pool, ws, stale, true, "received", days(120), json!({})).await?;
    // Another workspace's contact never appears.
    let foreign = target(&pool, other, "Foreign Zine", false).await?;
    message(&pool, other, foreign, true, "positive", days(1), json!({})).await?;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/control-plane/autopilot/reply-triage")
                .header(AUTHORIZATION, format!("Bearer {CONTROL_PLANE_KEY}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await?)?;

    let waiting = body["waiting_on_you"]
        .as_array()
        .expect("waiting_on_you is a list");
    let names: Vec<&str> = waiting
        .iter()
        .filter_map(|row| row["display_name"].as_str())
        .collect();
    assert_eq!(names, ["Night Radio", "Old Zine"], "{body}");
    assert_eq!(body["summary"]["waiting_on_you_count"], 2);
    assert_eq!(waiting[0]["disposition"], "positive");
    assert_eq!(waiting[0]["reply_label"], "POSITIVE");
    assert_eq!(waiting[1]["reply_label"], "Decision required");
    assert!(
        waiting[1]["last_written_at"].is_string(),
        "the act's last message is named: {body}"
    );
    assert!(
        waiting[1]["replied_at"]
            .as_str()
            .is_some_and(|at| at.contains('T')),
        "timestamps leave as RFC 3339 text: {body}"
    );
    // The classifier's sections are unchanged, and empty here.
    assert_eq!(body["needs_human"], json!([]));
    assert_eq!(body["summary"]["needs_human_count"], 0);
    Ok(())
}
