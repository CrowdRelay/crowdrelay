use super::load_social_content_history;
use crowdrelay_domain::WorkspaceId;
use serde_json::json;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn social_performance_keeps_mature_retention_separate_from_new_arrivals()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = PgPool::connect(&std::env::var("CROWDRELAY_TEST_DATABASE_URL")?).await?;
    let workspace = WorkspaceId::new();
    let other = WorkspaceId::new();
    let now = OffsetDateTime::now_utc();
    for tenant in [workspace, other] {
        sqlx::query("INSERT INTO workspaces (id,slug,name) VALUES ($1,$2,'Social yield proof')")
            .bind(tenant.into_uuid())
            .bind(format!("social-proof-{}", tenant.into_uuid().simple()))
            .execute(&pool)
            .await?;
    }
    for (tenant, days, opening) in [
        (workspace, 50, "mature"),
        (workspace, 5, "young"),
        (other, 50, "foreign"),
    ] {
        seed(&pool, tenant, now, days, opening).await?;
    }
    let history = load_social_content_history(&pool, workspace, now).await?;
    assert_eq!(
        history.len(),
        2,
        "the neighboring tenant stays out of the history"
    );
    let mature = history
        .iter()
        .find(|post| post.opening.as_deref() == Some("mature"))
        .ok_or("mature post missing")?;
    assert!(mature.retention_window_complete);
    assert_eq!(mature.fans_observed_30d, 4);
    assert_eq!(
        mature.fans_activated_within_30d, 1,
        "a later return does not erase activation"
    );
    assert_eq!(
        mature.fans_retained_30d, 1,
        "withdrawn consent, silence and future activity do not count"
    );
    assert_eq!(mature.retained_fans_per_1000_reach(), Some(1));
    let young = history
        .iter()
        .find(|post| post.opening.as_deref() == Some("young"))
        .ok_or("young post missing")?;
    assert!(!young.retention_window_complete);
    assert_eq!(young.fans_observed_30d, 0);
    assert_eq!(young.fans_retained_30d, 0);
    assert_eq!(young.retained_fans_per_1000_reach(), None);
    assert_eq!(history[0].opening.as_deref(), Some("mature"));
    Ok(())
}

async fn seed(
    pool: &PgPool,
    workspace: WorkspaceId,
    now: OffsetDateTime,
    days: i64,
    opening: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let source = Uuid::now_v7();
    let decision = Uuid::now_v7();
    let action = Uuid::now_v7();
    let event = Uuid::now_v7();
    let tenant = workspace.into_uuid();
    sqlx::query(
        "INSERT INTO content_sources (id,workspace_id,source_kind,source_key,title,metadata,occurred_at,expires_at)
         VALUES ($1,$2,'social_post',$3,'Real post',$4,$5,$5 + INTERVAL '180 days')",
    ).bind(source).bind(tenant).bind(source.to_string())
        .bind(json!({"platform":"instagram","body":opening,"reach":1000}))
        .bind(now - Duration::days(days)).execute(pool).await?;
    sqlx::query(
        "INSERT INTO autopilot_decisions
         (id,workspace_id,decision_key,context,subject_kind,subject_id,decision_kind,
          confidence_basis_points,disposition,reason,input_snapshot,policy_snapshot,recommendation,evaluated_at,trace_id)
         VALUES ($1,$2,$3,'growth_intelligence','workspace',$2,'proof',9000,'auto_execute','proof','{}','{}','{}',$4,$5)",
    ).bind(decision).bind(tenant).bind(decision.to_string()).bind(now).bind(Uuid::now_v7())
        .execute(pool).await?;
    sqlx::query(
        "INSERT INTO autopilot_actions
         (id,workspace_id,decision_id,context,action_kind,subject_kind,subject_id,idempotency_key,payload,status,finished_at)
         VALUES ($1,$2,$3,'growth_intelligence','agent.content.request','workspace',$2,$4,$5,'succeeded',$6)",
    ).bind(action).bind(tenant).bind(decision).bind(action.to_string())
        .bind(json!({"source_id":source})).bind(now).execute(pool).await?;
    sqlx::query(
        "INSERT INTO events (id,workspace_id,slug,title,starts_at,status,published_at)
         VALUES ($1,$2,$3,'Proof show',$4,'published',$5)",
    )
    .bind(event)
    .bind(tenant)
    .bind(event.to_string())
    .bind(now + Duration::days(10))
    .bind(now)
    .execute(pool)
    .await?;
    let conversion = now - Duration::days(days - 2);
    for ordinal in 0..4 {
        let fan = Uuid::now_v7();
        sqlx::query("INSERT INTO fans (id,workspace_id,normalized_email,status,created_at) VALUES ($1,$2,$3,'active',$4)")
            .bind(fan).bind(tenant).bind(format!("{fan}@example.test")).bind(conversion)
            .execute(pool).await?;
        sqlx::query(
            "INSERT INTO fan_provenance_events (workspace_id,fan_id,action_id,event_kind,channel,attribution_method,attribution_confidence,occurred_at)
             VALUES ($1,$2,$3,'conversion','instagram','last_tracked_click',1.0,$4)",
        ).bind(tenant).bind(fan).bind(action).bind(conversion).execute(pool).await?;
        sqlx::query("INSERT INTO fan_consents (workspace_id,fan_id,purpose,granted,policy_version,source,recorded_at) VALUES ($1,$2,'marketing',true,'v1','proof',$3)")
            .bind(tenant).bind(fan).bind(conversion).execute(pool).await?;
        if ordinal == 1 {
            sqlx::query("INSERT INTO fan_consents (workspace_id,fan_id,purpose,granted,policy_version,source,recorded_at) VALUES ($1,$2,'marketing',false,'v1','proof',$3)")
                .bind(tenant).bind(fan).bind(now - Duration::hours(1)).execute(pool).await?;
        }
        if ordinal == 0 {
            let early_event = Uuid::now_v7();
            sqlx::query("INSERT INTO events (id,workspace_id,slug,title,starts_at,status,published_at) VALUES ($1,$2,$3,'Early show',$4,'published',$5)")
                .bind(early_event).bind(tenant).bind(early_event.to_string())
                .bind(now + Duration::days(20)).bind(now).execute(pool).await?;
            sqlx::query("INSERT INTO event_interests (workspace_id,event_id,fan_id,created_at) VALUES ($1,$2,$3,$4)")
                .bind(tenant).bind(early_event).bind(fan).bind(conversion + Duration::hours(1))
                .execute(pool).await?;
        }
        if ordinal != 2 {
            let activity = if ordinal == 3 {
                now + Duration::hours(1)
            } else {
                now - Duration::hours(1)
            };
            sqlx::query("INSERT INTO event_interests (workspace_id,event_id,fan_id,created_at) VALUES ($1,$2,$3,$4)")
                .bind(tenant).bind(event).bind(fan).bind(activity).execute(pool).await?;
        }
    }
    Ok(())
}
