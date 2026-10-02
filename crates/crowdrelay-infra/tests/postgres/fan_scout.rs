//! FAN SCOUT prospect spine against the real migrated schema.
//!
//! The database proof pins the two boundaries that matter before any outreach
//! exists: rediscovery is idempotent and cannot erase a refusal; linking to
//! the owned fanbase only points at an already verified same-workspace fan.

use crate::common;
use crowdrelay_domain::{
    FanId, WorkspaceId,
    fan_scout::{FanProspectIdentity, FanProspectIdentityKind, FanProspectObservationKind},
};
use crowdrelay_infra::fan_scout::{ObserveProspectRequest, link_verified_fan, observe};
use time::OffsetDateTime;
use uuid::Uuid;

async fn workspace(
    pool: &sqlx::PgPool,
    label: &str,
) -> Result<WorkspaceId, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1,$2,$3)")
        .bind(id)
        .bind(format!("{label}-{}", id.simple()))
        .bind(label)
        .execute(pool)
        .await?;
    Ok(WorkspaceId::from_uuid(id))
}

fn observation(
    workspace_id: WorkspaceId,
    handle: &str,
    source_id: &str,
    observed_at: OffsetDateTime,
) -> ObserveProspectRequest {
    ObserveProspectRequest {
        workspace_id,
        identity: FanProspectIdentity::new("reddit", FanProspectIdentityKind::Handle, handle)
            .expect("valid handle"),
        display_name: Some(handle.trim_start_matches('@').to_owned()),
        profile_url: None,
        observation_kind: FanProspectObservationKind::PublicEngagement,
        source_kind: "community_comment".to_owned(),
        source_id: source_id.to_owned(),
        source_url: None,
        evidence: serde_json::json!({"body": "kiedy koncert?"}),
        observed_at,
    }
}

async fn active_fan(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    email: &str,
) -> Result<FanId, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status)
         VALUES ($1,$2,$3,'active')",
    )
    .bind(id)
    .bind(workspace_id.into_uuid())
    .bind(email)
    .execute(pool)
    .await?;
    Ok(FanId::from_uuid(id))
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn rediscovery_dedupes_and_never_clears_refusal() -> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool, "fan-scout-dedupe").await?;
    let now = OffsetDateTime::now_utc();

    let first = observe(&pool, observation(ws, "@MetalFanPL", "comment-1", now)).await?;
    let same = observe(
        &pool,
        observation(
            ws,
            "metalfanpl",
            "comment-1",
            now + time::Duration::minutes(1),
        ),
    )
    .await?;
    assert_eq!(first, same, "handle case and @ are one prospect");

    let observations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM scout_prospect_observations
         WHERE workspace_id=$1 AND prospect_id=$2",
    )
    .bind(ws.into_uuid())
    .bind(first.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(observations, 1, "the same source observation is idempotent");

    sqlx::query(
        "UPDATE scout_prospects SET status='refused'
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(ws.into_uuid())
    .bind(first.into_uuid())
    .execute(&pool)
    .await?;

    observe(
        &pool,
        observation(
            ws,
            "METALFANPL",
            "comment-2",
            now + time::Duration::minutes(2),
        ),
    )
    .await?;
    let status: String =
        sqlx::query_scalar("SELECT status FROM scout_prospects WHERE workspace_id=$1 AND id=$2")
            .bind(ws.into_uuid())
            .bind(first.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(status, "refused", "rediscovery cannot resurrect a refusal");
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn conversion_only_links_an_existing_verified_same_workspace_fan()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool, "fan-scout-link").await?;
    let foreign_ws = workspace(&pool, "fan-scout-foreign").await?;
    let now = OffsetDateTime::now_utc();

    let prospect = observe(&pool, observation(ws, "warm-person", "comment-a", now)).await?;
    let foreign_fan = active_fan(&pool, foreign_ws, "foreign@fan.test").await?;
    assert!(
        !link_verified_fan(&pool, ws, prospect, foreign_fan, now).await?,
        "tenant isolation is part of promotion"
    );

    let local_fan = active_fan(&pool, ws, "local@fan.test").await?;
    sqlx::query("DELETE FROM fan_identifiers WHERE workspace_id=$1 AND fan_id=$2")
        .bind(ws.into_uuid())
        .bind(local_fan.into_uuid())
        .execute(&pool)
        .await?;
    assert!(
        !link_verified_fan(&pool, ws, prospect, local_fan, now).await?,
        "an unverified public identity cannot be promoted into first-party fanhood"
    );

    sqlx::query(
        "INSERT INTO fan_identifiers
             (workspace_id, fan_id, kind, value, source, verified_at)
         VALUES ($1,$2,'email','local@fan.test','test',now())",
    )
    .bind(ws.into_uuid())
    .bind(local_fan.into_uuid())
    .execute(&pool)
    .await?;
    assert!(link_verified_fan(&pool, ws, prospect, local_fan, now).await?);

    let linked: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT status, linked_fan_id FROM scout_prospects
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(ws.into_uuid())
    .bind(prospect.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(linked.0, "converted");
    assert_eq!(linked.1, Some(local_fan.into_uuid()));
    Ok(())
}
