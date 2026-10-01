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
            .any(|e| e["eventId"].as_str() == Some(&event_id.to_string()))),
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

/// An approved, contactable beacon plus the ranked ask the brain parked for
/// the booker: decision row carrying the `north_star_ranking` belief, and
/// the `awaiting_approval` action the verbs act on.
async fn ranked_ask(
    pool: &PgPool,
    workspace: Uuid,
    city_id: Uuid,
    event_id: Uuid,
) -> Result<(Uuid, Uuid, Uuid), Box<dyn std::error::Error>> {
    let beacon_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO beacons (
            id, workspace_id, city_id, beacon_kind, display_name,
            contact_email, active, verified, accepts_outreach, do_not_contact
        ) VALUES ($1,$2,$3,'promoter','Klub Fabryka',
                  'booking@fabryka.example', true, true, true, false)
        "#,
    )
    .bind(beacon_id)
    .bind(workspace)
    .bind(city_id)
    .execute(pool)
    .await?;
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
           VALUES ($1,$2,$3,'beacon','beacon',$4,'amplify_local_signal',9000,
                   'require_approval','due ask',$5,'{}','{}',now(),$1)"#,
    )
    .bind(decision_id)
    .bind(workspace)
    .bind(format!("decision-{decision_id}"))
    .bind(beacon_id)
    .bind(json!({
        "north_star_ranking": {
            "estimation_regime": "y30_direct",
            "expected_incremental_y30": 2.4,
            "uncertainty": 0.8,
            "sample_size": 12,
            "uses_y30": true,
            "evidence_basis": "target_history",
            "opportunity_cost_fans": -0.9
        }
    }))
    .execute(pool)
    .await?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status, action_class)
           VALUES ($1,$2,$3,'beacon','beacon.outreach.request','beacon',$4,$5,$6,
                   'awaiting_approval','third_party')"#,
    )
    .bind(action_id)
    .bind(workspace)
    .bind(decision_id)
    .bind(beacon_id)
    .bind(format!("action-{action_id}"))
    .bind(json!({
        "kind": "request_beacon_outreach",
        "beacon_id": beacon_id,
        "event_id": event_id,
        "beacon_version": 1,
        "phase": "initial",
        "template_key": "beacon.local_story.v1",
    }))
    .execute(pool)
    .await?;
    Ok((beacon_id, decision_id, action_id))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_pending_ask_surfaces_its_ranking_and_answers_the_booker()
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
    let (beacon_id, decision_id, action_id) =
        ranked_ask(&pool, workspace_uuid, city_id, event_id).await?;
    sqlx::query(
        "INSERT INTO beacon_campaigns
            (workspace_id, beacon_id, event_id, status, last_reply_disposition, last_outreach_at)
         VALUES ($1, $2, $3, 'contacted', 'received', now() - interval '40 days')",
    )
    .bind(workspace_uuid)
    .bind(beacon_id)
    .bind(event_id)
    .execute(&pool)
    .await?;

    // The card carries the decision-time belief and the pair's own history —
    // the booker sees why this ask won before spending the approval.
    let view = get(&app).await?;
    let card = view["pendingAsks"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|r| r["actionId"].as_str() == Some(&action_id.to_string()))
        })
        .cloned()
        .expect("the ranked ask belongs on the pending-ask cards");
    assert_eq!(
        card["decisionId"].as_str(),
        Some(decision_id.to_string().as_str())
    );
    assert_eq!(
        card["beaconId"].as_str(),
        Some(beacon_id.to_string().as_str())
    );
    assert_eq!(
        card["eventId"].as_str(),
        Some(event_id.to_string().as_str())
    );
    assert_eq!(card["phase"].as_str(), Some("initial"));
    assert_eq!(card["templateKey"].as_str(), Some("beacon.local_story.v1"));
    assert_eq!(card["beaconKind"].as_str(), Some("promoter"));
    // The inner object is the ranker's snapshot verbatim — the same
    // decision-time keys the audit trail reads, not a re-shaped contract.
    assert_eq!(
        card["northStarRanking"]["estimation_regime"].as_str(),
        Some("y30_direct")
    );
    assert_eq!(
        card["northStarRanking"]["evidence_basis"].as_str(),
        Some("target_history")
    );
    assert_eq!(
        card["priorOutcome"]["status"].as_str(),
        Some("contacted"),
        "the pair's own history rides the card: {card}"
    );
    assert_eq!(
        card["priorOutcome"]["last_reply_disposition"].as_str(),
        Some("received")
    );
    assert!(card["daysToEvent"].as_i64().is_some());

    // `defer` — "not now": the campaign picks up the snooze and the ask
    // leaves the queue in the same answer.
    let (status, body) = post(
        &app,
        json!({"action": "defer", "beaconId": beacon_id, "eventId": event_id, "deferDays": 14}),
        &format!("defer-{beacon_id}"),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "defer must land: {body}");
    assert_eq!(body["status"].as_str(), Some("contacted"));
    assert_eq!(body["cancelledActions"].as_i64(), Some(1));
    assert!(body["deferredUntil"].as_str().is_some());
    assert_eq!(
        action_status(&pool, workspace_uuid, action_id).await?,
        "cancelled"
    );
    let deferred: Option<time::OffsetDateTime> = sqlx::query_scalar(
        "SELECT deferred_until FROM beacon_campaigns
         WHERE workspace_id = $1 AND beacon_id = $2 AND event_id = $3",
    )
    .bind(workspace_uuid)
    .bind(beacon_id)
    .bind(event_id)
    .fetch_one(&pool)
    .await?;
    assert!(deferred.is_some_and(|until| until > time::OffsetDateTime::now_utc()));

    // A retry of the same answer under the same key replays the stored
    // outcome — it does not stretch the window and it does not write a
    // second audit row.
    let (status, replay) = post(
        &app,
        json!({"action": "defer", "beaconId": beacon_id, "eventId": event_id, "deferDays": 14}),
        &format!("defer-{beacon_id}"),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "a retry replays: {replay}");
    assert_eq!(replay["replayed"].as_bool(), Some(true));
    let deferred_after: Option<time::OffsetDateTime> = sqlx::query_scalar(
        "SELECT deferred_until FROM beacon_campaigns
         WHERE workspace_id = $1 AND beacon_id = $2 AND event_id = $3",
    )
    .bind(workspace_uuid)
    .bind(beacon_id)
    .bind(event_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(deferred, deferred_after);

    // The same key under another verb on another pair is a collision, not
    // a replay — 409, not a silent merge.
    let (status, _) = post(
        &app,
        json!({"action": "decline", "beaconId": beacon_id, "eventId": event_id}),
        &format!("defer-{beacon_id}"),
    )
    .await?;
    assert_eq!(status, StatusCode::CONFLICT);

    // The deferred ask no longer sits on the card stack.
    let view = get(&app).await?;
    assert!(
        !view["pendingAsks"].as_array().is_some_and(|rows| rows
            .iter()
            .any(|r| r["actionId"].as_str() == Some(&action_id.to_string()))),
        "a deferred ask leaves the pending cards"
    );

    // `decline` — "not this pair": operator-made, distinguishable from a
    // partner's reply, and the pending-ask list stays empty of it.
    let (status, body) = post(
        &app,
        json!({"action": "decline", "beaconId": beacon_id, "eventId": event_id,
               "reason": "wrong partner for this show"}),
        &format!("decline-{beacon_id}"),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "decline must land: {body}");
    assert_eq!(body["status"].as_str(), Some("declined"));
    let (status_db, via): (String, Option<String>) = sqlx::query_as(
        "SELECT status, declined_via FROM beacon_campaigns
         WHERE workspace_id = $1 AND beacon_id = $2 AND event_id = $3",
    )
    .bind(workspace_uuid)
    .bind(beacon_id)
    .bind(event_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(status_db, "declined");
    assert_eq!(via.as_deref(), Some("operator"));

    // Missing pair identity is a bad request, not a silent no-op.
    let (status, _) = post(
        &app,
        json!({"action": "defer", "beaconId": beacon_id}),
        &format!("defer-bad-{beacon_id}"),
    )
    .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    Ok(())
}

async fn action_status(
    pool: &PgPool,
    workspace: Uuid,
    action_id: Uuid,
) -> Result<String, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT status FROM autopilot_actions WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace)
    .bind(action_id)
    .fetch_one(pool)
    .await?)
}
