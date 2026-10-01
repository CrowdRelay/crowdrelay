//! Bulk archive promote through the real router.
//!
//! The count check is the feature's safety catch: the operator confirms a
//! number, and a segment that drifted between render and click must answer
//! 409 with both numbers — never widen the send. Only the routed request can
//! prove the endpoint reads the table at write time, and only a real
//! transaction can prove import + marks commit together.
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

const CONTROL_PLANE_KEY: &str = "test-control-plane-key-123456789012";

async fn post(
    app: &axum::Router,
    uri: &str,
    body: Value,
) -> Result<(StatusCode, Value), Box<dyn std::error::Error>> {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(AUTHORIZATION, format!("Bearer {CONTROL_PLANE_KEY}"))
                .header(CONTENT_TYPE, "application/json")
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

async fn seed_drive_contact(
    pool: &PgPool,
    workspace: Uuid,
    email: &str,
    suggested_kind: Option<&str>,
    city: Option<&str>,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO drive_contacts
            (id, workspace_id, normalized_email, suggested_kind, city,
             source_file_id, source_file_name, sources)
        VALUES ($1, $2, $3, $4, $5, 'file-1', 'contacts.csv', '{gdrive}')
        "#,
    )
    .bind(id)
    .bind(workspace)
    .bind(email)
    .bind(suggested_kind)
    .bind(city)
    .execute(pool)
    .await?;
    Ok(id)
}

/// The stale-count refusal: the number the operator confirmed is checked
/// against the table at write time, and a drift is a 409 naming both sides.
/// Then the same click with the true count promotes exactly the likely-fan
/// segment — pending fan, double-opt-in outbox row, staged row marked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn promote_batch_rejects_a_stale_count_then_promotes_the_segment()
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

    // The sheet itself declared this row a fan — bare mailbox domains and
    // inbound replies no longer qualify a contact for the bulk wave.
    let fan_row =
        seed_drive_contact(&pool, workspace_uuid, "basia@gmail.com", Some("fan"), None).await?;
    let _org = seed_drive_contact(&pool, workspace_uuid, "bookings@klubx.pl", None, None).await?;
    let _venue = seed_drive_contact(
        &pool,
        workspace_uuid,
        "room@stodola.pl",
        Some("venue"),
        None,
    )
    .await?;

    let uri = "/v1/control-plane/gdrive/contacts/promote-batch";

    // The page said 5; the table holds 1. The send must not widen.
    let (status, body) = post(
        &app,
        uri,
        json!({
            "destination": "fan",
            "segment": "likely_fan",
            "expected_count": 5,
            "reason": "Przenosimy listę do Signal",
        }),
    )
    .await?;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a stale count must 409: {body}"
    );
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("1") && detail.contains("5"),
        "the conflict names both numbers: {detail}"
    );

    // A nonsense destination or segment is a 400, not a partial import.
    for bad in [
        json!({"destination": "beacon", "segment": "likely_fan", "expected_count": 1}),
        json!({"destination": "fan", "segment": "likely_org", "expected_count": 1}),
        json!({"destination": "fan", "segment": "likely_fan", "expected_count": -1}),
        json!({"destination": "fan", "segment": "likely_fan", "expected_count": 1, "limit": 0}),
        json!({"destination": "fan", "segment": "likely_fan", "expected_count": 1, "limit": 100_001}),
    ] {
        let (status, _) = post(&app, uri, bad.clone()).await?;
        assert_eq!(status, StatusCode::BAD_REQUEST, "bad request: {bad}");
    }

    // The confirmed click: one likely fan imported pending + marked, in one
    // transaction; the org and the venue stay staged.
    let (status, body) = post(
        &app,
        uri,
        json!({
            "destination": "fan",
            "segment": "likely_fan",
            "expected_count": 1,
            "reason": "Przenosimy listę do Signal",
        }),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "promote-batch failed: {body}");
    assert_eq!(body["promoted"], 1);
    assert_eq!(body["imported_pending"], 1);
    assert_eq!(body["skipped_suppressed"], 0);

    let fan_status: String = sqlx::query_scalar(
        "SELECT status FROM fans WHERE workspace_id = $1 AND normalized_email = 'basia@gmail.com'",
    )
    .bind(workspace_uuid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(fan_status, "pending", "consent is never bypassed");

    let payload: Value = sqlx::query_scalar(
        "SELECT payload FROM outbox_events \
         WHERE workspace_id = $1 AND event_type = 'fan.confirmation_requested'",
    )
    .bind(workspace_uuid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        payload["invitation"]["reason"],
        "Przenosimy listę do Signal"
    );
    assert_eq!(payload["invitation"]["source_label"], "archive");
    assert!(
        payload["locale"].is_null(),
        "no crew locale set — null, not a guess: {payload}"
    );

    let outcomes: Vec<(String, String)> = sqlx::query_as(
        "SELECT normalized_email, fan_outcome FROM drive_contacts \
         WHERE workspace_id = $1 ORDER BY normalized_email",
    )
    .bind(workspace_uuid)
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        outcomes,
        vec![
            ("basia@gmail.com".to_owned(), "promoted".to_owned()),
            ("bookings@klubx.pl".to_owned(), "staged".to_owned()),
            ("room@stodola.pl".to_owned(), "staged".to_owned()),
        ],
        "only the likely-fan row moved"
    );
    let marked: String = sqlx::query_scalar(
        "SELECT fan_outcome FROM drive_contacts WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_uuid)
    .bind(fan_row)
    .fetch_one(&pool)
    .await?;
    assert_eq!(marked, "promoted");

    // The list endpoint answers the segment cut and its counts.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/control-plane/gdrive/contacts?segment=likely_fan")
                .header(AUTHORIZATION, format!("Bearer {CONTROL_PLANE_KEY}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await?;
    let listed: Value = serde_json::from_slice(&bytes)?;
    assert_eq!(
        listed["segment_counts"]["likely_org"], 1,
        "the org row still counts: {listed}"
    );
    assert_eq!(listed["segment_counts"]["likely_fan"], 0);
    // `decided` means closed on BOTH axes — the promoted row's beacon side
    // is still staged, so it is not decided yet.
    assert_eq!(listed["segment_counts"]["decided"], 0);
    assert_eq!(listed["contacts"].as_array().map(Vec::len), Some(0));

    Ok(())
}

/// The bounded wave: `limit` takes the top evidence slice — an inbound
/// history outranks a dual-source sighting, which outranks a bare row —
/// and `expected_count` names that slice, so a growing segment still 409s
/// only when the wave itself drifted. A sheet city the catalogue
/// recognises lands on the fan as a `fan_city_interests` row: the band's
/// knowledge, not a notification opt-in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn promote_batch_limit_takes_the_evidence_slice_and_carries_city()
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

    // The catalogue already seeds Wrocław — the test's city must be a real
    // row, and the slug-name resolve path is what the promote uses.
    let city_id: Uuid =
        sqlx::query_scalar("SELECT id FROM cities WHERE country_code = 'PL' AND slug = 'wroclaw'")
            .fetch_one(&pool)
            .await?;

    // Three staged likely-fan rows in reverse evidence order — each is
    // sheet-declared 'fan' (qualification is declared, not inferred from the
    // mailbox provider); the wave's evidence ordering still applies inside
    // the qualified set.
    let bare =
        seed_drive_contact(&pool, workspace_uuid, "bare@gmail.com", Some("fan"), None).await?;
    let dual =
        seed_drive_contact(&pool, workspace_uuid, "dual@gmail.com", Some("fan"), None).await?;
    sqlx::query("UPDATE drive_contacts SET sources = '{gdrive,gmail}' WHERE id = $1")
        .bind(dual)
        .execute(&pool)
        .await?;
    let inbound = seed_drive_contact(
        &pool,
        workspace_uuid,
        "inbound@gmail.com",
        Some("fan"),
        None,
    )
    .await?;
    sqlx::query(
        "UPDATE drive_contacts SET last_inbound_at = now(), city = 'Wrocław' WHERE id = $1",
    )
    .bind(inbound)
    .execute(&pool)
    .await?;

    let uri = "/v1/control-plane/gdrive/contacts/promote-batch";

    // The wave is capped at two — the bare row stays staged.
    let (status, body) = post(
        &app,
        uri,
        json!({
            "destination": "fan",
            "segment": "likely_fan",
            "expected_count": 2,
            "limit": 2,
            "reason": "Przenosimy listę do Signal",
        }),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "bounded promote failed: {body}");
    assert_eq!(body["promoted"], 2);

    let outcomes: Vec<(String, String)> = sqlx::query_as(
        "SELECT normalized_email, fan_outcome FROM drive_contacts \
         WHERE workspace_id = $1 ORDER BY normalized_email",
    )
    .bind(workspace_uuid)
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        outcomes,
        vec![
            ("bare@gmail.com".to_owned(), "staged".to_owned()),
            ("dual@gmail.com".to_owned(), "promoted".to_owned()),
            ("inbound@gmail.com".to_owned(), "promoted".to_owned()),
        ],
        "evidence order: inbound history + dual source outrank the bare row"
    );

    // The inbound row's sheet city resolved through the catalogue and
    // reached the fan as an interest — never a location preference.
    let interest: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fan_city_interests interest \
         JOIN fans fan ON fan.workspace_id = interest.workspace_id \
            AND fan.id = interest.fan_id \
         WHERE interest.workspace_id = $1 \
           AND fan.normalized_email = 'inbound@gmail.com' \
           AND interest.city_id = $2",
    )
    .bind(workspace_uuid)
    .bind(city_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(interest, 1, "the sheet city became the fan's interest");
    let preferences: i64 =
        sqlx::query_scalar("SELECT count(*) FROM fan_location_preferences WHERE workspace_id = $1")
            .bind(workspace_uuid)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        preferences, 0,
        "a sheet city never opts a fan into nearby pushes"
    );

    // The leftover row promotes on the next wave — count re-confirmed.
    let (status, body) = post(
        &app,
        uri,
        json!({
            "destination": "fan",
            "segment": "likely_fan",
            "expected_count": 1,
        }),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "tail wave failed: {body}");
    assert_eq!(body["promoted"], 1);
    let bare_outcome: String =
        sqlx::query_scalar("SELECT fan_outcome FROM drive_contacts WHERE id = $1")
            .bind(bare)
            .fetch_one(&pool)
            .await?;
    assert_eq!(bare_outcome, "promoted");

    Ok(())
}

/// The beacon half of the batch: every row is promoted through the same
/// kind-routed methods the per-row endpoint calls — `radio` becomes a
/// proposed outreach target, `venue` files under its resolved booking
/// city. A `fan`-typed row is claimed by `likely_fan` — the beacon segment
/// never lists it, so the wave cannot touch it — and the untyped org cut
/// falls back to the same `press` default the single promote applies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn promote_batch_beacon_routes_kinds_and_keeps_unpromotable_rows_staged()
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

    let radio = seed_drive_contact(
        &pool,
        workspace_uuid,
        "radio@radiokampus.pl",
        Some("radio"),
        None,
    )
    .await?;
    let venue = seed_drive_contact(
        &pool,
        workspace_uuid,
        "room@stodola.pl",
        Some("venue"),
        Some("wroclaw"),
    )
    .await?;
    let fan_typed =
        seed_drive_contact(&pool, workspace_uuid, "kasia@o2.pl", Some("fan"), None).await?;
    let org = seed_drive_contact(&pool, workspace_uuid, "biuro@fundacja.pl", None, None).await?;

    let uri = "/v1/control-plane/gdrive/contacts/promote-batch";

    // A fan-segment row can never batch through the beacon lane, and a
    // drifted count still 409s with both numbers.
    let (status, _) = post(
        &app,
        uri,
        json!({"destination": "beacon", "segment": "likely_fan", "expected_count": 3}),
    )
    .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = post(
        &app,
        uri,
        json!({"destination": "beacon", "segment": "beacon", "expected_count": 99}),
    )
    .await?;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a stale beacon count must 409: {body}"
    );
    // An unknown kind override is a caller bug, not a press contact.
    let (status, _) = post(
        &app,
        uri,
        json!({"destination": "beacon", "segment": "beacon", "expected_count": 2, "kind": "goblin"}),
    )
    .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The confirmed beacon wave: radio → proposed outreach target, venue →
    // booking route at its resolved city.
    let (status, body) = post(
        &app,
        uri,
        json!({"destination": "beacon", "segment": "beacon", "expected_count": 2}),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "beacon batch failed: {body}");
    assert_eq!(body["promoted"], 2, "radio + venue promote: {body}");

    let target_kind: String = sqlx::query_scalar(
        "SELECT target_kind FROM agent_outreach_targets \
         WHERE workspace_id = $1 AND contact_email = 'radio@radiokampus.pl'",
    )
    .bind(workspace_uuid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(target_kind, "radio");
    let target_status: String = sqlx::query_scalar(
        "SELECT status FROM agent_outreach_targets \
         WHERE workspace_id = $1 AND contact_email = 'radio@radiokampus.pl'",
    )
    .bind(workspace_uuid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(target_status, "proposed", "screening still owns the send");

    // The untyped org files under the same press default as the per-row
    // promote — proposed, awaiting the screening loop.
    let (status, body) = post(
        &app,
        uri,
        json!({"destination": "beacon", "segment": "likely_org", "expected_count": 1}),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "org batch failed: {body}");
    assert_eq!(body["promoted"], 1);
    let org_kind: String = sqlx::query_scalar(
        "SELECT target_kind FROM agent_outreach_targets \
         WHERE workspace_id = $1 AND contact_email = 'biuro@fundacja.pl'",
    )
    .bind(workspace_uuid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(org_kind, "press");

    // Outcome axis: promoted rows marked, the fan-typed row still staged
    // on the beacon side.
    let outcomes: Vec<(String, String)> = sqlx::query_as(
        "SELECT normalized_email, beacon_outcome FROM drive_contacts \
         WHERE workspace_id = $1 ORDER BY normalized_email",
    )
    .bind(workspace_uuid)
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        outcomes,
        vec![
            ("biuro@fundacja.pl".to_owned(), "promoted".to_owned()),
            ("kasia@o2.pl".to_owned(), "staged".to_owned()),
            ("radio@radiokampus.pl".to_owned(), "promoted".to_owned()),
            ("room@stodola.pl".to_owned(), "promoted".to_owned()),
        ],
        "every promoted row marked, the refused kind untouched"
    );
    for id in [radio, venue, org] {
        let marked: String =
            sqlx::query_scalar("SELECT beacon_outcome FROM drive_contacts WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(marked, "promoted");
    }
    let fan_side_stays: String =
        sqlx::query_scalar("SELECT fan_outcome FROM drive_contacts WHERE id = $1")
            .bind(fan_typed)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        fan_side_stays, "staged",
        "the beacon wave never touches the fan axis"
    );

    Ok(())
}
