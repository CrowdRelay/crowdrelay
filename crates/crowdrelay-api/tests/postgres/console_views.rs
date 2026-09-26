//! The console view reads: one request per page, and the page's honesty
//! rules held by the query rather than by the browser.
//!
//! Driven through the real routes against a migrated database. The rules
//! under test are the ones a copy in the browser used to get wrong: a source
//! past `expires_at` is not usable even while `active` stays true, and a night
//! with no ticket sale or no door campaign reads null, never zero.

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

async fn get(
    pool: &PgPool,
    workspace_id: Uuid,
    uri: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    let app = crowdrelay_api::router(
        app_state(pool, WorkspaceId::from_uuid(workspace_id))?,
        HttpConfig::new(["http://localhost:4321".to_owned()])?,
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(AUTHORIZATION, format!("Bearer {CONTROL_PLANE_KEY}"))
                .body(Body::empty())?,
        )
        .await?;
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "{uri}: {}",
        String::from_utf8_lossy(&body)
    );
    Ok(serde_json::from_slice(&body)?)
}

async fn source(
    pool: &PgPool,
    workspace_id: Uuid,
    kind: &str,
    title: &str,
    expires_in_days: i32,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO content_sources
             (id, workspace_id, source_kind, source_key, title, occurred_at, expires_at, metadata)
         VALUES ($1, $2, $3, $4, $5, now() - interval '40 days',
                 now() + make_interval(days => $6), '{}'::jsonb)",
    )
    .bind(id)
    .bind(workspace_id)
    .bind(kind)
    .bind(format!("{kind}:{}", id.simple()))
    .bind(title)
    .bind(expires_in_days)
    .execute(pool)
    .await?;
    Ok(id)
}

async fn content_supply_use(
    pool: &PgPool,
    workspace_id: Uuid,
    source_id: Uuid,
) -> Result<(), Box<dyn std::error::Error>> {
    let decision_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO autopilot_decisions
             (id, workspace_id, decision_key, context, subject_kind, subject_id,
              decision_kind, confidence_basis_points, disposition, reason,
              input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
         VALUES ($1,$2,$3,'content_supply','content_source',$4,
                 'supply_content',9000,'require_approval','seeded use',
                 '{}','{}','{}',now(),$5) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(format!("console-view-decision-{}", Uuid::now_v7()))
    .bind(source_id)
    .bind(Uuid::now_v7())
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO autopilot_actions
             (id, workspace_id, decision_id, context, action_kind, subject_kind,
              subject_id, idempotency_key, payload, status, finished_at)
         VALUES ($1,$2,$3,'content_supply','content.post.request','content_source',
                 $4,$5,'{}'::jsonb,'succeeded',now())",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(decision_id)
    .bind(source_id)
    .bind(format!("console-view-action-{}", Uuid::now_v7()))
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn material_counts_expired_sources_as_aged_out_and_folds_song_copies()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let workspace_id = seed_workspace(&pool).await?;
    let used = source(&pool, workspace_id, "video", "Live from the club", 30).await?;
    // Past its expiry, still flagged active: aged out, not usable.
    source(&pool, workspace_id, "video", "Old rehearsal", -5).await?;
    // Three copies of one song: single, album track, live edition.
    source(&pool, workspace_id, "release", "Rise", 30).await?;
    source(&pool, workspace_id, "release", "09 - Virya - Rise", 30).await?;
    source(&pool, workspace_id, "release", "Rise (Live)", 30).await?;
    content_supply_use(&pool, workspace_id, used).await?;
    content_supply_use(&pool, workspace_id, used).await?;

    let view = get(
        &pool,
        workspace_id,
        "/v1/control-plane/views/content-material",
    )
    .await?;
    assert_eq!(view["total"], 5, "{view}");
    assert_eq!(view["usable"], 4, "the expired source is aged out: {view}");
    assert_eq!(view["used"], 1, "{view}");
    assert_eq!(view["uses_total"], 2, "{view}");
    let kinds = view["by_kind"].as_array().ok_or("by_kind is a list")?;
    let release = kinds
        .iter()
        .find(|kind| kind["kind"] == "release")
        .ok_or("a release row")?;
    assert_eq!(release["total"], 3, "{release}");
    assert_eq!(
        release["distinct_titles"], 1,
        "three copies of one song: {release}"
    );
    assert_eq!(view["recent"].as_array().map(Vec::len), Some(5), "{view}");
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_city_night_nobody_measured_reads_null_not_zero() -> Result<(), Box<dyn std::error::Error>>
{
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let workspace_id = seed_workspace(&pool).await?;
    let city_slug = format!("city-{}", Uuid::now_v7().simple());
    let city_id: Uuid = sqlx::query_scalar(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ($1, 'Testowo', 'PL', 51.1, 17.03) RETURNING id",
    )
    .bind(&city_slug)
    .fetch_one(&pool)
    .await?;
    let event_slug = format!("night-{}", Uuid::now_v7().simple());
    let event_id: Uuid = sqlx::query_scalar(
        "INSERT INTO events (workspace_id, slug, title, starts_at, status, published_at, city_id)
         VALUES ($1, $2, 'Club night', now() - interval '10 days', 'published', now(), $3)
         RETURNING id",
    )
    .bind(workspace_id)
    .bind(&event_slug)
    .bind(city_id)
    .fetch_one(&pool)
    .await?;
    let fan_id: Uuid = sqlx::query_scalar(
        "INSERT INTO fans (workspace_id, normalized_email, status)
         VALUES ($1, $2, 'active') RETURNING id",
    )
    .bind(workspace_id)
    .bind(format!("interested-{}@example.test", workspace_id.simple()))
    .fetch_one(&pool)
    .await?;
    sqlx::query("INSERT INTO event_interests (workspace_id, event_id, fan_id) VALUES ($1, $2, $3)")
        .bind(workspace_id)
        .bind(event_id)
        .bind(fan_id)
        .execute(&pool)
        .await?;

    let view = get(
        &pool,
        workspace_id,
        &format!("/v1/control-plane/views/cities/{city_slug}"),
    )
    .await?;
    let last = &view["last_show"];
    assert_eq!(last["slug"], event_slug.as_str(), "{view}");
    assert_eq!(last["interested"], 1, "{view}");
    assert!(
        last["checkins"].is_null(),
        "no door campaign: not measured, {view}"
    );
    assert!(
        last["paid_buyers"].is_null(),
        "no ticket sale: not measured, {view}"
    );
    assert!(view["funnel"].is_null(), "no fan named the city: {view}");
    assert_eq!(view["rooms"].as_array().map(Vec::len), Some(0), "{view}");

    // The door read for the same night: unticketed, so tickets are null.
    let door = get(
        &pool,
        workspace_id,
        &format!("/v1/control-plane/events/{event_slug}/scan"),
    )
    .await?;
    assert!(door["event"]["tickets_sold"].is_null(), "{door}");
    assert_eq!(door["event"]["interested"], 1, "{door}");
    assert_eq!(door["event"]["city"], "Testowo", "{door}");

    // The show list carries the same night with the same honesty.
    let list = get(&pool, workspace_id, "/v1/control-plane/events").await?;
    let night = list["events"]
        .as_array()
        .and_then(|events| events.iter().find(|e| e["slug"] == event_slug.as_str()))
        .ok_or("the night is on the list")?;
    assert!(night["tickets_sold"].is_null(), "{night}");
    assert_eq!(night["interested"], 1, "{night}");
    assert_eq!(night["door_campaigns"], 0, "{night}");
    assert_eq!(night["city"], "Testowo", "{night}");

    // And the timeline the show page opens on.
    let timeline = get(
        &pool,
        workspace_id,
        &format!("/v1/control-plane/events/{event_slug}/timeline"),
    )
    .await?;
    assert_eq!(timeline["event"]["interested"], 1, "{timeline}");
    assert_eq!(timeline["event"]["city"], "Testowo", "{timeline}");
    Ok(())
}
