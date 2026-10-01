//! The scout-to-operator surface through the real router.
//!
//! An `event_network_scout` beacon enters the review queue unverified and
//! non-contactable. The queue row carries what the operator needs to judge
//! it — the model's `whyFit`, the evidence snippet the model actually saw,
//! the match history and the one next step the row is waiting on. Approval
//! stamps `network_review`; only then may a per-partner tracked link be
//! minted for the pilot.
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
use tower::ServiceExt;
use uuid::Uuid;

const ADMIN_KEY: &str = "test-admin-api-key-123456789012";
const URI: &str = "/v1/admin/autopilot/beacon-network";

async fn post(
    app: &axum::Router,
    body: Value,
    idempotency_key: &str,
) -> Result<(StatusCode, Value), Box<dyn std::error::Error>> {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(URI)
                .header(AUTHORIZATION, format!("Bearer {ADMIN_KEY}"))
                .header(CONTENT_TYPE, "application/json")
                .header("idempotency-key", idempotency_key)
                .body(Body::from(body.to_string()))?,
        )
        .await?;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await?;
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)?
    };
    Ok((status, json))
}

async fn get(app: &axum::Router) -> Result<Value, Box<dyn std::error::Error>> {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(URI)
                .header(AUTHORIZATION, format!("Bearer {ADMIN_KEY}"))
                .body(Body::empty())?,
        )
        .await?;
    let bytes = to_bytes(response.into_body(), usize::MAX).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn city(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ($1, 'Scout City', 'PL', 52.23, 21.01) RETURNING id",
    )
    .bind(format!("scout-city-{}", Uuid::now_v7().simple()))
    .fetch_one(pool)
    .await?)
}

async fn event(
    pool: &PgPool,
    workspace: Uuid,
    city_id: Uuid,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, city_id, slug, title, starts_at, status, published_at)
         VALUES ($1, $2, $3, $4, 'A show', now() + interval '30 days', 'published', now())",
    )
    .bind(id)
    .bind(workspace)
    .bind(city_id)
    .bind(format!("scout-show-{}", id.simple()))
    .execute(pool)
    .await?;
    Ok(id)
}

/// A beacon exactly as the worker's scout intake writes it: active,
/// unverified, non-contactable, carrying the `event_network_scout` block the
/// validator produced — including the evidence proof and the match reason.
async fn scout_beacon(
    pool: &PgPool,
    workspace: Uuid,
    city_id: Uuid,
    event_id: Uuid,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO beacons (
            id, workspace_id, city_id, beacon_kind, display_name,
            contact_email, destination_url, source_url,
            active, verified, accepts_outreach, do_not_contact, metadata
        ) VALUES ($1,$2,$3,'radio','Radio Ostrów',
                  'redakcja@radio-ostrow.pl','https://radio-ostrow.pl/kontakt',
                  'https://radio-ostrow.pl/kontakt',
                  true,false,false,false,$4)
        "#,
    )
    .bind(id)
    .bind(workspace)
    .bind(city_id)
    .bind(json!({
        "event_network_scout": {
            "event_id": event_id,
            "agent_outcome_id": Uuid::now_v7(),
            "source_url": "https://radio-ostrow.pl/kontakt",
            "why_fit": "Local radio covering the show's city",
            "evidence": {
                "snippet": "Radio Ostrów lokalna rozgłośnia — kontakt z redakcją.",
                "tool": "web_search",
                "fetched_at": "2026-10-01T12:00:00Z"
            },
            "human_review_required": true,
            "marketing_email_consent_confirmed": false
        }
    }))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO beacon_event_matches (workspace_id, beacon_id, event_id) VALUES ($1,$2,$3)",
    )
    .bind(workspace)
    .bind(id)
    .bind(event_id)
    .execute(pool)
    .await?;
    Ok(id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn scout_candidate_reviews_then_earns_a_tracked_partner_link()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let workspace_uuid = attestation_anchor::seed_workspace(&pool).await?;
    let workspace_id = WorkspaceId::from_uuid(workspace_uuid);
    let app = crowdrelay_api::router(
        attestation_anchor::app_state(&pool, workspace_id)?,
        crowdrelay_api::HttpConfig::new(["http://localhost:4321".to_owned()])?,
    );

    let city_id = city(&pool).await?;
    let event_id = event(&pool, workspace_uuid, city_id).await?;
    let beacon_id = scout_beacon(&pool, workspace_uuid, city_id, event_id).await?;

    // The review queue surfaces what the operator needs: the reason the
    // model matched this node, the snippet it actually read, which shows it
    // matched against, and the step this row waits on.
    let view = get(&app).await?;
    let pending = &view["pendingCandidates"];
    let row = pending
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|r| r["id"].as_str() == Some(&beacon_id.to_string()))
        })
        .expect("the scout beacon belongs on the review queue");
    assert_eq!(
        row["whyFit"].as_str(),
        Some("Local radio covering the show's city")
    );
    assert_eq!(
        row["evidenceSnippet"].as_str(),
        Some("Radio Ostrów lokalna rozgłośnia — kontakt z redakcją.")
    );
    assert_eq!(row["nextStep"].as_str(), Some("review_candidate"));
    assert!(
        row["matchedEvents"].as_array().is_some_and(|events| events
            .iter()
            .any(|e| e["event_id"].as_str() == Some(&event_id.to_string()))),
        "matched events must name the show the scout researched: {row}"
    );

    // A tracked link is a launched action: minting before approval refuses.
    let (status, _) = post(
        &app,
        json!({"action": "partner_link", "beaconId": beacon_id, "eventId": event_id}),
        &format!("pl-early-{beacon_id}"),
    )
    .await?;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "an unapproved beacon must not get a tracked link"
    );

    // Operator verifies the source and confirms consent — the pilot path.
    let (status, body) = post(
        &app,
        json!({
            "action": "approve",
            "beaconId": beacon_id,
            "sourceVerified": true,
            "marketingEmailConsentConfirmed": true,
            "consentEvidenceUrl": "https://radio-ostrow.pl/kontakt"
        }),
        &format!("approve-{beacon_id}"),
    )
    .await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "scout candidate must approve: {body}"
    );

    // The approved row now waits on the invite step — and the queue shows it.
    let view = get(&app).await?;
    let approved = view["approvedCandidates"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|r| r["id"].as_str() == Some(&beacon_id.to_string()))
        })
        .cloned()
        .expect("approved scout beacon belongs on the invite-eligible list");
    assert_eq!(approved["nextStep"].as_str(), Some("send_invite"));
    assert_eq!(
        approved["networkReview"]["source_verified"].as_bool(),
        Some(true),
        "the review record must be visible on the row: {approved}"
    );

    // Mint the partner's tracked link — the default destination is the
    // event's public page.
    let (status, body) = post(
        &app,
        json!({"action": "partner_link", "beaconId": beacon_id, "eventId": event_id}),
        &format!("pl-{beacon_id}"),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "partner link must mint: {body}");
    assert_eq!(body["alreadyExisted"].as_bool(), Some(false));
    let url = body["url"].as_str().expect("the link url").to_owned();
    assert!(
        url.starts_with("http://localhost:4321/l/bp-"),
        "the link lives under the tenant's tracked /l/ path: {url}"
    );

    let link: Option<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT slug, channel_source, channel_creative FROM smart_links \
         WHERE workspace_id = $1 AND slug = $2",
    )
    .bind(workspace_uuid)
    .bind(body["slug"].as_str().unwrap())
    .fetch_optional(&pool)
    .await?;
    let (_, channel_source, channel_creative) = link.expect("the minted smart_link row must exist");
    assert_eq!(channel_source.as_deref(), Some("beacon_partner"));
    assert_eq!(
        channel_creative.as_deref(),
        Some(beacon_id.to_string().as_str()),
        "clicks attribute to the partner who carried them"
    );

    // The beacon row itself now advertises the link — no second lookup.
    let links: Value = sqlx::query_scalar(
        "SELECT metadata->'partner_links' FROM beacons WHERE workspace_id=$1 AND id=$2",
    )
    .bind(workspace_uuid)
    .bind(beacon_id)
    .fetch_one(&pool)
    .await?;
    assert!(
        links.as_array().is_some_and(|rows| !rows.is_empty()),
        "the beacon row must record its partner link"
    );

    // Replay returns the same link rather than splitting attribution.
    let (status, replay) = post(
        &app,
        json!({"action": "partner_link", "beaconId": beacon_id, "eventId": event_id}),
        &format!("pl-again-{beacon_id}"),
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        replay["url"].as_str(),
        Some(url.as_str()),
        "a repeat request returns the link the partner already has"
    );
    assert_eq!(replay["alreadyExisted"].as_bool(), Some(true));
    Ok(())
}
