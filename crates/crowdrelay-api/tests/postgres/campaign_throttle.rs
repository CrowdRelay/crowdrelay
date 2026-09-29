//! The 72-hour fan email gap, through the real router.
//!
//! A fan with a delivery row — claimed, delivered or failed — on another
//! campaign inside `FAN_EMAIL_MIN_GAP_HOURS` is left out of a new campaign's
//! recipient snapshot. The mail's fate does not matter to the throttle: a
//! claim-expired send may have left and may not have, and "nearly mailed" is
//! close enough to mailed for spam protection.
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

/// Inserts the two feature flags the delivery-plan gate reads before it
/// touches anything else.
async fn enable_campaign_flags(
    pool: &PgPool,
    workspace: Uuid,
) -> Result<(), Box<dyn std::error::Error>> {
    for key in ["communication_campaigns_enabled", "mailer_enabled"] {
        sqlx::query(
            "INSERT INTO ecosystem_feature_flags (workspace_id, key, enabled, reason)
             VALUES ($1, $2, true, 'test')
             ON CONFLICT (workspace_id, key) DO UPDATE SET enabled = true",
        )
        .bind(workspace)
        .bind(key)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn seed_fan(
    pool: &PgPool,
    workspace: Uuid,
    email: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let fan_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status)
         VALUES ($1, $2, $3, 'active')",
    )
    .bind(fan_id)
    .bind(workspace)
    .bind(email)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_consents
           (id, workspace_id, fan_id, purpose, granted, policy_version, source)
         VALUES ($1, $2, $3, 'marketing', true, 'v1', 'test')",
    )
    .bind(Uuid::now_v7())
    .bind(workspace)
    .bind(fan_id)
    .execute(pool)
    .await?;
    Ok(fan_id)
}

/// A `scheduled` campaign carries `dispatch_event_id` NOT NULL by CHECK —
/// the outbox row is the schedule's proof, so the fixture writes one.
async fn seed_scheduled_campaign(
    pool: &PgPool,
    workspace: Uuid,
    segment_id: Uuid,
    slug: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let outbox_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO outbox_events (id, workspace_id, event_type, payload)
         VALUES ($1, $2, 'communication.campaign_due', '{}')",
    )
    .bind(outbox_id)
    .bind(workspace)
    .execute(pool)
    .await?;
    let campaign_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO communication_campaigns
           (id, workspace_id, segment_id, slug, name, channel, template_key,
            status, scheduled_at, dispatch_event_id)
         VALUES ($1, $2, $3, $4, 'Throttle probe', 'email', 'release.sustain.v1',
                 'scheduled', now() - interval '1 minute', $5)",
    )
    .bind(campaign_id)
    .bind(workspace)
    .bind(segment_id)
    .bind(slug)
    .bind(outbox_id)
    .execute(pool)
    .await?;
    Ok(campaign_id)
}

async fn delivery_plan(
    app: &axum::Router,
    campaign_id: Uuid,
) -> Result<(StatusCode, Value), Box<dyn std::error::Error>> {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/v1/internal/communications/campaigns/{campaign_id}/delivery-plan?limit=100"
                ))
                .header(
                    AUTHORIZATION,
                    format!("Bearer {}", attestation_anchor::COMMERCE_KEY),
                )
                .header(CONTENT_TYPE, "application/json")
                .body(Body::empty())?,
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

/// Campaign A claims a fan an hour ago; campaign B goes out a minute later.
/// The fresh delivery must keep the fan out of B's snapshot — the claim,
/// not the send result, is what the gap measures. The same fan with a
/// four-day-old delivery and a fan nobody mailed both stay reachable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_recently_mailed_fan_is_not_snapshotted_again_within_72h()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let workspace = attestation_anchor::seed_workspace(&pool).await?;
    let workspace_id = WorkspaceId::from_uuid(workspace);
    let app = crowdrelay_api::router(
        attestation_anchor::app_state(&pool, workspace_id)?,
        crowdrelay_api::HttpConfig::new(["http://localhost:4321".to_owned()])?,
    );
    enable_campaign_flags(&pool, workspace).await?;

    let segment_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO audience_segments (id, workspace_id, slug, name, filter, active)
         VALUES ($1, $2, 'throttle-all', 'Throttle probe segment', '{}', true)",
    )
    .bind(segment_id)
    .bind(workspace)
    .execute(&pool)
    .await?;

    // Three fans: fresh-claim (throttled), old-claim (gap lapsed), never.
    let fresh = seed_fan(&pool, workspace, "fresh-claim@x.test").await?;
    let lapsed = seed_fan(&pool, workspace, "lapsed-claim@x.test").await?;
    let _never = seed_fan(&pool, workspace, "never-mailed@x.test").await?;

    let campaign_a = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO communication_campaigns
           (id, workspace_id, segment_id, slug, name, channel, template_key, status)
         VALUES ($1, $2, $3, 'camp-earlier', 'Earlier wave', 'email',
                 'release.release_day.v1', 'draft')",
    )
    .bind(campaign_a)
    .bind(workspace)
    .bind(segment_id)
    .execute(&pool)
    .await?;
    for (fan_id, claimed_ago) in [(fresh, "1 hour"), (lapsed, "96 hours")] {
        sqlx::query(
            "INSERT INTO communication_campaign_recipients
               (workspace_id, campaign_id, fan_id) VALUES ($1, $2, $3)",
        )
        .bind(workspace)
        .bind(campaign_a)
        .bind(fan_id)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO communication_campaign_deliveries
               (workspace_id, campaign_id, fan_id, attempt_key, status, claimed_at)
             VALUES ($1, $2, $3, $4, 'claimed', now() - $5::interval)",
        )
        .bind(workspace)
        .bind(campaign_a)
        .bind(fan_id)
        .bind(format!("attempt-{fan_id}"))
        .bind(claimed_ago)
        .execute(&pool)
        .await?;
    }

    let campaign_b = seed_scheduled_campaign(&pool, workspace, segment_id, "camp-next").await?;
    let (status, plan) = delivery_plan(&app, campaign_b).await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "delivery-plan refused: {}",
        serde_json::to_string(&plan).unwrap_or_default()
    );

    let reached: Vec<String> = plan["recipients"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row["email"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !reached.iter().any(|email| email == "fresh-claim@x.test"),
        "a fan claimed an hour ago must not be snapshotted again: {reached:?}"
    );
    assert!(
        reached.iter().any(|email| email == "lapsed-claim@x.test"),
        "a claim 96h old is outside the gap and must not exclude: {reached:?}"
    );
    assert!(
        reached.iter().any(|email| email == "never-mailed@x.test"),
        "a fan nobody mailed stays reachable: {reached:?}"
    );

    // The snapshot is the recipient list, not just the page — count what was
    // actually written so an off-list leak shows up too.
    let snapshotted: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM communication_campaign_recipients
         WHERE workspace_id = $1 AND campaign_id = $2",
    )
    .bind(workspace)
    .bind(campaign_b)
    .fetch_one(&pool)
    .await?;
    assert_eq!(snapshotted, 2);
    Ok(())
}

/// The throttle applies to `email` only. A push campaign over the same
/// audience snapshots the just-claimed fan: pushes collapse by key and do
/// not carry the spam-reading cost email does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_gap_does_not_apply_to_push_campaigns() -> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let workspace = attestation_anchor::seed_workspace(&pool).await?;
    let workspace_id = WorkspaceId::from_uuid(workspace);
    let app = crowdrelay_api::router(
        attestation_anchor::app_state(&pool, workspace_id)?,
        crowdrelay_api::HttpConfig::new(["http://localhost:4321".to_owned()])?,
    );
    enable_campaign_flags(&pool, workspace).await?;

    let segment_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO audience_segments (id, workspace_id, slug, name, filter, active)
         VALUES ($1, $2, 'throttle-push', 'Throttle push segment', '{}', true)",
    )
    .bind(segment_id)
    .bind(workspace)
    .execute(&pool)
    .await?;

    let fresh = seed_fan(&pool, workspace, "fresh-claim-push@x.test").await?;
    let campaign_a = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO communication_campaigns
           (id, workspace_id, segment_id, slug, name, channel, template_key, status)
         VALUES ($1, $2, $3, 'camp-earlier-push', 'Earlier wave', 'email',
                 'release.release_day.v1', 'draft')",
    )
    .bind(campaign_a)
    .bind(workspace)
    .bind(segment_id)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO communication_campaign_recipients
           (workspace_id, campaign_id, fan_id) VALUES ($1, $2, $3)",
    )
    .bind(workspace)
    .bind(campaign_a)
    .bind(fresh)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO communication_campaign_deliveries
           (workspace_id, campaign_id, fan_id, attempt_key, status, claimed_at)
         VALUES ($1, $2, $3, $4, 'claimed', now() - interval '1 hour')",
    )
    .bind(workspace)
    .bind(campaign_a)
    .bind(fresh)
    .bind(format!("attempt-{fresh}"))
    .execute(&pool)
    .await?;

    // Push campaigns go through `push/enqueue` for materialization, but the
    // recipient snapshot is shared — `delivery-plan` snapshots for it too
    // (channel 'push' still serves the plan read). The gap must not apply.
    let campaign_b = Uuid::now_v7();
    let outbox_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO outbox_events (id, workspace_id, event_type, payload)
         VALUES ($1, $2, 'communication.campaign_due', '{}')",
    )
    .bind(outbox_id)
    .bind(workspace)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO communication_campaigns
           (id, workspace_id, segment_id, slug, name, channel, template_key,
            status, scheduled_at, dispatch_event_id)
         VALUES ($1, $2, $3, 'camp-push', 'Push wave', 'push',
                 'release.release_day.v1', 'scheduled',
                 now() - interval '1 minute', $4)",
    )
    .bind(campaign_b)
    .bind(workspace)
    .bind(segment_id)
    .bind(outbox_id)
    .execute(&pool)
    .await?;

    let (status, plan) = delivery_plan(&app, campaign_b).await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "delivery-plan refused: {}",
        serde_json::to_string(&plan).unwrap_or_default()
    );
    let snapshotted: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM communication_campaign_recipients
         WHERE workspace_id = $1 AND campaign_id = $2",
    )
    .bind(workspace)
    .bind(campaign_b)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        snapshotted,
        1,
        "push snapshots must not apply the email gap: {}",
        json!(plan)
    );
    Ok(())
}
