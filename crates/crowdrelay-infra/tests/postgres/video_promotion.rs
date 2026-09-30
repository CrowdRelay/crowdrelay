//! Real database proofs for promotion policy and truthful artifact receipts.
use super::autopilot_dispatch_envelope::{Fixture, seed_outcome_action, setup};
use crowdrelay_application::autopilot::{
    AutopilotActionRepository, AutopilotRuntimeRepository, ClaimExecution, ExecutorReportStatus,
    RecordExecutionReport,
};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

async fn video(f: &Fixture) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar("INSERT INTO content_sources(workspace_id,source_kind,source_key,title,occurred_at,expires_at,metadata) VALUES($1,'video',$2,'Video',now()-interval '60 days',now()+interval '30 days',$3) RETURNING id")
        .bind(f.workspace_id.into_uuid()).bind(Uuid::now_v7().to_string())
        .bind(json!({"url":"https://www.youtube.com/watch?v=test"})).fetch_one(&f.pool).await?)
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database"]
async fn a_notification_receipt_cannot_complete_an_artifact()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let source = video(&f).await?;
    let now = OffsetDateTime::now_utc();
    let id = seed_outcome_action(
        &f,
        "content.artifact.request",
        json!({
            "kind":"request_content_artifact", "source_id":source, "source_version":1,
            "artifact":"social_feed", "template_key":"content.social_feed.v1"
        }),
        now,
    )
    .await?;
    let actions = f
        .repository
        .claim_due_autonomous_actions(f.workspace_id, 8, now)
        .await?;
    let action = actions
        .iter()
        .find(|action| action.id.into_uuid() == id)
        .expect("claimed");
    f.repository
        .execute_action(f.workspace_id, action, now)
        .await?;
    let claim = f
        .repository
        .claim_execution(
            f.workspace_id,
            ClaimExecution {
                action_id: id.into(),
                executor_id: "notification-test".to_owned(),
                occurred_at: now,
            },
        )
        .await?;
    f.repository.record_execution_report(f.workspace_id, RecordExecutionReport {
        action_id:id.into(), receipt_key:format!("notification-{id}"), executor_id:"notification-test".to_owned(),
        status:ExecutorReportStatus::Succeeded, claim_token:claim.claim_token,
        provider_reference:Some("discord:message-1".to_owned()), error_kind:None,
        metadata:json!({"provider":"discord", "event":"crowdrelay.content.artifact_requested"}), occurred_at:now,
    }).await?;
    let (status, error): (String, Option<String>) = sqlx::query_as(
        "SELECT status,last_error_kind FROM autopilot_actions WHERE workspace_id=$1 AND id=$2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(status, "failed");
    assert_eq!(error.as_deref(), Some("artifact_delivery_missing"));
    let emitted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM autopilot_action_emissions WHERE workspace_id=$1 AND action_id=$2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(
        emitted, 1,
        "dispatch intent stays auditable; it is not artifact delivery"
    );
    let delivered: i64 = sqlx::query_scalar("SELECT count(*) FROM autopilot_execution_reports WHERE workspace_id=$1 AND action_id=$2 AND status='succeeded'")
        .bind(f.workspace_id.into_uuid()).bind(id).fetch_one(&f.pool).await?;
    assert_eq!(
        delivered, 0,
        "the request receipt cannot become successful artifact evidence"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database"]
async fn promotion_persists_valid_exclusions_and_reports_only_permitted_lanes()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let source = video(&f).await?;
    let alias: Uuid = sqlx::query_scalar("INSERT INTO content_sources(workspace_id,source_kind,source_key,title,occurred_at,expires_at,metadata) VALUES($1,'release','video-alias','Video release',now(),now()+interval '30 days',$2) RETURNING id")
        .bind(f.workspace_id.into_uuid()).bind(json!({"listen_url":"https://youtu.be/test"})).fetch_one(&f.pool).await?;
    let response =
        crowdrelay_infra::autopilot::request_drop_surge(&f.pool, f.workspace_id, source, None)
            .await?;
    assert!(response.excluded_platforms.contains(&"facebook".to_owned()));
    assert!(!response.lanes.contains(&"instagram".to_owned()));
    let response = crowdrelay_infra::autopilot::request_drop_surge(
        &f.pool,
        f.workspace_id,
        source,
        Some(vec!["telegram".to_owned(), "forum".to_owned()]),
    )
    .await?;
    assert!(!response.lanes.contains(&"telegram".to_owned()));
    assert!(
        response.lanes.contains(&"facebook".to_owned()),
        "explicit policy is authoritative"
    );
    let metadata: serde_json::Value =
        sqlx::query_scalar("SELECT metadata FROM content_sources WHERE workspace_id=$1 AND id=$2")
            .bind(f.workspace_id.into_uuid())
            .bind(source)
            .fetch_one(&f.pool)
            .await?;
    assert_eq!(
        metadata["promotion_excluded_platforms"],
        json!(["telegram", "forum"])
    );
    let policy: serde_json::Value = sqlx::query_scalar("SELECT metadata->'promotion_excluded_platforms' FROM content_sources WHERE workspace_id=$1 AND id=$2")
        .bind(f.workspace_id.into_uuid()).bind(alias).fetch_one(&f.pool).await?;
    assert_eq!(
        policy,
        json!(["telegram", "forum"]),
        "release aliases cannot bypass video exclusions"
    );
    let inherited: serde_json::Value = sqlx::query_scalar("INSERT INTO content_sources(workspace_id,source_kind,source_key,title,occurred_at,expires_at,metadata) VALUES($1,'release','later-alias','Later alias',now(),now()+interval '30 days',$2) RETURNING metadata->'promotion_excluded_platforms'")
        .bind(f.workspace_id.into_uuid()).bind(json!({"listen_url":"https://www.youtube.com/watch?v=test&feature=share"})).fetch_one(&f.pool).await?;
    assert_eq!(inherited, policy, "later aliases inherit asset policy");
    assert!(
        crowdrelay_infra::autopilot::request_drop_surge(
            &f.pool,
            f.workspace_id,
            source,
            Some(vec!["typo".to_owned()])
        )
        .await
        .is_err()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database"]
async fn queued_actions_recheck_live_source_policy_and_workspace_ownership()
-> Result<(), Box<dyn std::error::Error>> {
    let f = setup().await?;
    let source = video(&f).await?;
    let id = seed_outcome_action(
        &f,
        "community.engage.request",
        json!({"draft":{"source_id":source}}),
        OffsetDateTime::now_utc(),
    )
    .await?;
    let allowed = |platform: &'static str| {
        crowdrelay_infra::promotion_policy::action_platform_allowed(
            &f.pool,
            f.workspace_id.into_uuid(),
            id,
            platform,
        )
    };
    assert!(!allowed("facebook").await?);
    assert!(allowed("forum").await?);
    sqlx::query("UPDATE content_sources SET metadata=jsonb_set(metadata,'{promotion_excluded_platforms}',$3) WHERE workspace_id=$1 AND id=$2")
        .bind(f.workspace_id.into_uuid()).bind(source).bind(json!(["forum"])).execute(&f.pool).await?;
    assert!(
        !allowed("forum").await?,
        "policy changed after drafting must win"
    );
    assert!(allowed("facebook").await?);
    assert!(
        !crowdrelay_infra::promotion_policy::action_platform_allowed(
            &f.pool,
            Uuid::now_v7(),
            id,
            "forum"
        )
        .await?
    );
    assert!(
        !crowdrelay_infra::promotion_policy::source_platform_allowed(
            &f.pool,
            Uuid::now_v7(),
            source,
            "forum"
        )
        .await?
    );
    Ok(())
}
