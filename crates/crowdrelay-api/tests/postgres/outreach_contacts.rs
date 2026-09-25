//! The outreach conversation list, and "I wrote back".
//!
//! Production on 2026-09-25 held 399 outreach contacts: 16 whose latest
//! message was theirs, 111 waiting on an answer, 271 never written to, one
//! closed. None of it was listable, and an act that answered from its own
//! mailbox had no way to say so. What only a routed request against a real
//! schema proves: the four states and their order, the counts over every
//! contact whatever the filter, the filters themselves, and the write that
//! moves a conversation out of "your turn" — idempotent, refused for a
//! do-not-contact contact, and reflected in the reply triage view.
//!
//! Runs under `just test-postgres` against `CROWDRELAY_TEST_DATABASE_URL`.

use crate::{attestation_anchor, common};

use axum::{
    body::{Body, to_bytes},
    http::{
        Request, StatusCode,
        header::{AUTHORIZATION, CONTENT_TYPE},
    },
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
    kind: &str,
    do_not_contact: bool,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO outreach_targets
             (id, workspace_id, target_kind, display_name, contact_email, do_not_contact)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(workspace)
    .bind(kind)
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
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO outreach_interactions
             (workspace_id, target_id, direction, phase, disposition, source_key, occurred_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(workspace)
    .bind(target)
    .bind(if inbound { "inbound" } else { "outbound" })
    .bind(if inbound { "reply" } else { "initial" })
    .bind(disposition)
    .bind(format!("test:{}", Uuid::now_v7()))
    .bind(at)
    .execute(pool)
    .await?;
    Ok(())
}

async fn get(
    app: &axum::Router,
    uri: &str,
) -> Result<(StatusCode, Value), Box<dyn std::error::Error>> {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(AUTHORIZATION, format!("Bearer {CONTROL_PLANE_KEY}"))
                .body(Body::empty())?,
        )
        .await?;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await?;
    Ok((
        status,
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)?
        },
    ))
}

async fn written(
    app: &axum::Router,
    target: Uuid,
    key: &str,
    at: OffsetDateTime,
) -> Result<(StatusCode, Value), Box<dyn std::error::Error>> {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/control-plane/autopilot/outreach-targets/{target}/written"))
                .header(AUTHORIZATION, format!("Bearer {CONTROL_PLANE_KEY}"))
                .header(CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", key)
                .body(Body::from(
                    json!({ "occurred_at": at.format(&time::format_description::well_known::Rfc3339)? })
                        .to_string(),
                ))?,
        )
        .await?;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await?;
    Ok((
        status,
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)?
        },
    ))
}

fn names(body: &Value) -> Vec<String> {
    body["contacts"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row["display_name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn conversations_list_by_state_and_a_written_back_contact_moves_on()
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

    // Your turn: an old received answer, and a newer positive one that leads.
    let zine = target(&pool, ws, "Old Zine", "press", false).await?;
    message(&pool, ws, zine, false, "none", days(100)).await?;
    message(&pool, ws, zine, true, "received", days(95)).await?;
    let radio = target(&pool, ws, "Night Radio", "radio", false).await?;
    message(&pool, ws, radio, false, "none", days(3)).await?;
    message(&pool, ws, radio, true, "positive", days(2)).await?;
    // Waiting on them: the act wrote last.
    let club = target(&pool, ws, "Club Room", "support_slot", false).await?;
    message(&pool, ws, club, false, "none", days(6)).await?;
    // Never written to.
    let _fresh = target(&pool, ws, "Fresh Blog", "press", false).await?;
    // Closed: a refusal, and a do-not-contact contact that answered.
    let fest = target(&pool, ws, "Declining Fest", "press", false).await?;
    message(&pool, ws, fest, false, "none", days(4)).await?;
    message(&pool, ws, fest, true, "declined", days(3)).await?;
    let dnc = target(&pool, ws, "Do Not Write", "press", true).await?;
    message(&pool, ws, dnc, true, "received", days(1)).await?;
    // Another workspace's contact never appears.
    let foreign = target(&pool, other, "Foreign Zine", "press", false).await?;
    message(&pool, other, foreign, true, "positive", days(1)).await?;

    let (status, body) = get(&app, "/v1/control-plane/autopilot/outreach-contacts").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        names(&body),
        [
            "Night Radio",
            "Old Zine",
            "Club Room",
            "Fresh Blog",
            "Declining Fest",
            "Do Not Write"
        ],
        "{body}"
    );
    assert_eq!(
        body["counts"],
        json!({"your_turn": 2, "waiting_on_them": 1, "not_contacted": 1, "closed": 2, "total": 6})
    );
    let first = &body["contacts"][0];
    assert_eq!(first["state"], "your_turn");
    assert_eq!(first["answer_disposition"], "positive");
    assert_eq!(first["messages_sent"], 1);
    assert_eq!(first["answers"], 1);
    assert_eq!(
        body["contacts"][2]["answer_disposition"],
        Value::Null,
        "the act spoke last"
    );

    // Filters cut the list, never the counts.
    let (_, filtered) = get(
        &app,
        "/v1/control-plane/autopilot/outreach-contacts?state=your_turn&kind=press",
    )
    .await?;
    assert_eq!(names(&filtered), ["Old Zine"]);
    assert_eq!(filtered["counts"]["total"], 6);
    let (bad, _) = get(
        &app,
        "/v1/control-plane/autopilot/outreach-contacts?state=maybe",
    )
    .await?;
    assert_eq!(bad, StatusCode::BAD_REQUEST);

    // "I wrote back" moves the old answer to waiting-on-them, once.
    let (status, reply) = written(&app, zine, "written-zine-1", now).await?;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["replayed"], false);
    let (status, replay) = written(&app, zine, "written-zine-1", now).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay["replayed"], true);
    let outbound: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outreach_interactions
         WHERE workspace_id = $1 AND target_id = $2 AND direction = 'outbound'",
    )
    .bind(ws)
    .bind(zine)
    .fetch_one(&pool)
    .await?;
    assert_eq!(outbound, 2, "one message logged, not two");
    let phase: String = sqlx::query_scalar(
        "SELECT phase FROM outreach_interactions
         WHERE workspace_id = $1 AND target_id = $2 AND source_key LIKE 'operator:%'",
    )
    .bind(ws)
    .bind(zine)
    .fetch_one(&pool)
    .await?;
    assert_eq!(phase, "followup");
    let (_, after) = get(&app, "/v1/control-plane/autopilot/outreach-contacts").await?;
    assert_eq!(after["counts"]["your_turn"], 1);
    assert_eq!(after["counts"]["waiting_on_them"], 2);

    // The reply triage view agrees: only the radio is still waiting on the act.
    let (_, triage) = get(&app, "/v1/control-plane/autopilot/reply-triage").await?;
    assert_eq!(triage["summary"]["waiting_on_you_count"], 1);

    // Writing to a do-not-contact contact is refused, and nothing is logged.
    let (status, _) = written(&app, dnc, "written-dnc-1", now).await?;
    assert_eq!(status, StatusCode::CONFLICT);
    let dnc_outbound: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outreach_interactions
         WHERE workspace_id = $1 AND target_id = $2 AND direction = 'outbound'",
    )
    .bind(ws)
    .bind(dnc)
    .fetch_one(&pool)
    .await?;
    assert_eq!(dnc_outbound, 0);

    // A time in the future is not a message that was sent.
    let (status, _) = written(&app, radio, "written-future", now + Duration::days(1)).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    Ok(())
}
