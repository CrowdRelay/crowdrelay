use crate::common;

use crowdrelay_application::{FanIdentityRepository, MergeFansCommand};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::fan_identity::PgFanIdentityRepository;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

async fn fan(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
    created_at: OffsetDateTime,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans(id,workspace_id,normalized_email,status,created_at)
         VALUES($1,$2,$3,'active',$4)",
    )
    .bind(id)
    .bind(workspace_id)
    .bind(email)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(id)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn retention_and_engagement_follow_exact_canonical_identity_after_merge() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let workspace = WorkspaceId::new();
    let w = workspace.into_uuid();
    sqlx::query("INSERT INTO workspaces(id,slug,name) VALUES($1,$2,'retention identity')")
        .bind(w)
        .bind(format!("retention-identity-{}", w.simple()))
        .execute(&pool)
        .await?;

    let acquired_at = OffsetDateTime::now_utc() - Duration::days(40);
    let survivor = fan(&pool, w, "retention-survivor@example.test", acquired_at).await?;
    let historical = fan(&pool, w, "retention-historical@example.test", acquired_at).await?;
    let referred = fan(
        &pool,
        w,
        "retention-referred@example.test",
        OffsetDateTime::now_utc() - Duration::days(5),
    )
    .await?;

    sqlx::query(
        "INSERT INTO fan_consents(
             workspace_id,fan_id,purpose,granted,policy_version,source,recorded_at
         ) VALUES($1,$2,'marketing',true,'v1','test',$3)",
    )
    .bind(w)
    .bind(historical)
    .bind(acquired_at)
    .execute(&pool)
    .await?;

    let code: Uuid = sqlx::query_scalar(
        "INSERT INTO referral_codes(workspace_id,fan_id,code)
         VALUES($1,$2,$3) RETURNING id",
    )
    .bind(w)
    .bind(historical)
    .bind(format!("retention{}", historical.simple()))
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO referral_attributions(
             workspace_id,referrer_fan_id,referred_fan_id,referral_code_id,
             accepted_at,status,qualified_at,qualification_reason
         ) VALUES(
             $1,$2,$3,$4,$5,'qualified',$5,'confirmed_fan_signup'
         )",
    )
    .bind(w)
    .bind(historical)
    .bind(referred)
    .bind(code)
    .bind(OffsetDateTime::now_utc() - Duration::days(5))
    .execute(&pool)
    .await?;

    let identity = PgFanIdentityRepository::new(pool.clone());
    identity
        .merge_fans(&MergeFansCommand {
            workspace_id: workspace,
            survivor_fan_id: survivor,
            merged_fan_id: historical,
            reason: Some("same person".to_owned()),
            merged_by: "retention-test".to_owned(),
            request_id: "retention-identity-merge".to_owned(),
        })
        .await?;

    let observed_at = OffsetDateTime::now_utc();
    let retained_from_historical: bool =
        sqlx::query_scalar("SELECT fan_is_meaningfully_retained($1,$2,$3,$4)")
            .bind(w)
            .bind(historical)
            .bind(acquired_at)
            .bind(observed_at)
            .fetch_one(&pool)
            .await?;
    assert!(
        retained_from_historical,
        "a pinned historical fan id must resolve to the live canonical person"
    );

    let engagement_from_survivor: bool =
        sqlx::query_scalar("SELECT fan_has_engagement_between($1,$2,$3,$4,$5)")
            .bind(w)
            .bind(survivor)
            .bind("retention-survivor@example.test")
            .bind(observed_at - Duration::days(10))
            .bind(observed_at + Duration::microseconds(1))
            .fetch_one(&pool)
            .await?;
    assert!(
        engagement_from_survivor,
        "deliberate referral evidence pinned to the historical row must be visible from the survivor"
    );

    // A valid session touch is evidence that the person opened the product, but
    // it is deliberately not sufficient for the funnel's stronger engagement
    // proof. This prevents a session heartbeat from masquerading as activation.
    let session_only = fan(&pool, w, "retention-session-only@example.test", acquired_at).await?;
    let mut token_hash = session_only.as_bytes().to_vec();
    token_hash.extend_from_slice(session_only.as_bytes());
    sqlx::query(
        "INSERT INTO fan_sessions(
             workspace_id,fan_id,session_token_hash,created_at,last_seen_at,expires_at
         ) VALUES($1,$2,$3,$4,$5,$6)",
    )
    .bind(w)
    .bind(session_only)
    .bind(token_hash)
    .bind(acquired_at)
    .bind(observed_at - Duration::days(1))
    .bind(observed_at + Duration::days(30))
    .execute(&pool)
    .await?;

    let session_is_engagement: bool =
        sqlx::query_scalar("SELECT fan_has_engagement_between($1,$2,$3,$4,$5)")
            .bind(w)
            .bind(session_only)
            .bind("retention-session-only@example.test")
            .bind(observed_at - Duration::days(10))
            .bind(observed_at + Duration::microseconds(1))
            .fetch_one(&pool)
            .await?;
    assert!(
        !session_is_engagement,
        "session activity alone must never become deliberate funnel engagement"
    );
    Ok(())
}
