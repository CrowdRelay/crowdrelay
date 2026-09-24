//! §4e-7 #1: the scan step says what the room produced, not only how many
//! phones pointed at the door.
//!
//! Driven through the real route and the real query. The split rests on one
//! join — a check-in created its fan when that fan's provenance carries a
//! `concert_qr` conversion for the same campaign — and a copy of the SQL in
//! the test would prove only that the copy agrees with itself.

use crate::attestation_anchor::{app_state, seed_workspace};
use crate::common;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header::AUTHORIZATION},
};
use crowdrelay_api::HttpConfig;
use crowdrelay_domain::WorkspaceId;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

/// The control-plane key `app_state` installs (as its SHA-256).
const CONTROL_PLANE_KEY: &str = "test-control-plane-key-123456789012";

struct Room {
    workspace_id: Uuid,
    event_id: Uuid,
    campaign_id: Uuid,
}

/// One scanning fan: a fan row in `status`, an optional marketing-consent
/// history (oldest first), a check-in through the room's campaign, and — when
/// the scan created them — the arrival conversion `record_fan_arrival` writes,
/// tagged with `arrival_campaign`.
async fn scan(
    pool: &PgPool,
    room: &Room,
    label: &str,
    status: &str,
    consents: &[bool],
    arrival_campaign: Option<Uuid>,
) -> Result<(), Box<dyn std::error::Error>> {
    let fan_id: Uuid = sqlx::query_scalar(
        "INSERT INTO fans (workspace_id, normalized_email, status) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(room.workspace_id)
    .bind(format!("{label}-{}@example.test", room.workspace_id.simple()))
    .bind(status)
    .fetch_one(pool)
    .await?;
    for (step, granted) in consents.iter().enumerate() {
        sqlx::query(
            "INSERT INTO fan_consents
                 (workspace_id, fan_id, purpose, granted, policy_version, source, recorded_at)
             VALUES ($1, $2, 'marketing', $3, 'v1', 'concert_qr',
                     now() - make_interval(mins => $4))",
        )
        .bind(room.workspace_id)
        .bind(fan_id)
        .bind(granted)
        // Later entries are more recent: the latest decision is the one that
        // counts, as in `fan_activation_kpi`.
        .bind(i32::try_from(consents.len() - step)?)
        .execute(pool)
        .await?;
    }
    if let Some(campaign_id) = arrival_campaign {
        sqlx::query(
            "INSERT INTO fan_provenance_events
                 (workspace_id, fan_id, event_kind, channel, campaign_id, occurred_at)
             VALUES ($1, $2, 'conversion', 'concert_qr', $3, now())",
        )
        .bind(room.workspace_id)
        .bind(fan_id)
        .bind(campaign_id)
        .execute(pool)
        .await?;
    }
    sqlx::query(
        "INSERT INTO concert_checkins
             (workspace_id, event_id, campaign_id, fan_id, identity_source)
         VALUES ($1, $2, $3, $4, 'email_claim')",
    )
    .bind(room.workspace_id)
    .bind(room.event_id)
    .bind(room.campaign_id)
    .bind(fan_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_scan_splits_the_room_into_new_reachable_and_unconfirmed()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let workspace_id = seed_workspace(&pool).await?;
    let slug = format!("room-{}", Uuid::now_v7().simple());
    let event_id: Uuid = sqlx::query_scalar(
        "INSERT INTO events (workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1, $2, 'Club night', now() - INTERVAL '2 hours', 'published', now())
         RETURNING id",
    )
    .bind(workspace_id)
    .bind(&slug)
    .fetch_one(&pool)
    .await?;
    let mut campaigns = Vec::new();
    for label in ["door", "last-month"] {
        campaigns.push(
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO concert_qr_campaigns
                     (workspace_id, event_id, label, valid_from, valid_until)
                 VALUES ($1, $2, $3, now() - INTERVAL '3 hours', now() + INTERVAL '3 hours')
                 RETURNING id",
            )
            .bind(workspace_id)
            .bind(event_id)
            .bind(label)
            .fetch_one(&pool)
            .await?,
        );
    }
    let room = Room {
        workspace_id,
        event_id,
        campaign_id: campaigns[0],
    };
    let door = Some(room.campaign_id);

    // New, confirmed and consented: the number §4e-6 is about.
    scan(&pool, &room, "new-reachable", "active", &[true], door).await?;
    // New, gave an address, never confirmed it.
    scan(&pool, &room, "new-unconfirmed", "pending", &[true], door).await?;
    // New, but the latest consent decision revoked the first: new, not reachable.
    scan(&pool, &room, "new-revoked", "active", &[true, false], door).await?;
    // A fan the band already had, scanning again: counted, but not new.
    scan(&pool, &room, "returning", "active", &[true], None).await?;
    // Created by a different campaign — another night's door. Not this room's.
    scan(
        &pool,
        &room,
        "elsewhere",
        "active",
        &[true],
        Some(campaigns[1]),
    )
    .await?;

    let app = crowdrelay_api::router(
        app_state(&pool, WorkspaceId::from_uuid(workspace_id))?,
        HttpConfig::new(["http://localhost:4321".to_owned()])?,
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/control-plane/events/{slug}/timeline"))
                .header(AUTHORIZATION, format!("Bearer {CONTROL_PLANE_KEY}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await?)?;
    let scan_step = body["steps"]
        .as_array()
        .and_then(|steps| steps.iter().find(|step| step["key"] == "the_scan"))
        .ok_or("the timeline carries the scan step")?;
    let detail = &scan_step["detail"];
    assert_eq!(detail["checkins"], 5, "{detail}");
    assert_eq!(detail["new_fans"], 3, "created by this door: {detail}");
    assert_eq!(
        detail["new_reachable"], 1,
        "active and consented now: {detail}"
    );
    assert_eq!(detail["new_unconfirmed"], 1, "{detail}");
    Ok(())
}
