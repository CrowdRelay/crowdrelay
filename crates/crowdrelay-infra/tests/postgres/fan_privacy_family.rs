use crate::common;

use crowdrelay_application::{FanIdentityRepository, MergeFansCommand};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{
    fan_identity::PgFanIdentityRepository,
    fan_privacy::PostgresFanPrivacyRepository,
};
use sqlx::PgPool;
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

async fn fan(pool: &PgPool, workspace: Uuid, email: &str) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans(id,workspace_id,normalized_email,status)
         VALUES($1,$2,$3,'active')",
    )
    .bind(id)
    .bind(workspace)
    .bind(email)
    .execute(pool)
    .await?;
    Ok(id)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn erasure_scrubs_the_entire_merged_identity_family() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let workspace = WorkspaceId::new();
    let w = workspace.into_uuid();
    sqlx::query("INSERT INTO workspaces(id,slug,name) VALUES($1,$2,'privacy family')")
        .bind(w)
        .bind(format!("privacy-family-{}", w.simple()))
        .execute(&pool)
        .await?;

    let survivor = fan(&pool, w, "privacy-survivor@example.test").await?;
    let child = fan(&pool, w, "privacy-child@example.test").await?;
    let referred = fan(&pool, w, "privacy-referred@example.test").await?;

    sqlx::query(
        "INSERT INTO fan_consents(
             workspace_id,fan_id,purpose,granted,policy_version,source
         ) VALUES($1,$2,'marketing',true,'v1','test')",
    )
    .bind(w)
    .bind(child)
    .execute(&pool)
    .await?;

    let code: Uuid = sqlx::query_scalar(
        "INSERT INTO referral_codes(workspace_id,fan_id,code)
         VALUES($1,$2,$3) RETURNING id",
    )
    .bind(w)
    .bind(child)
    .bind(format!("privacy{}", child.simple()))
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO referral_attributions(
             workspace_id,referrer_fan_id,referred_fan_id,referral_code_id,
             accepted_at,status,qualified_at
         ) VALUES($1,$2,$3,$4,now(),'qualified',now())",
    )
    .bind(w)
    .bind(child)
    .bind(referred)
    .bind(code)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_acquisition_events(
             workspace_id,fan_id,source,request_id,referral_code_id,referrer_fan_id
         ) VALUES($1,$2,'public_signup',$3,$4,$5)",
    )
    .bind(w)
    .bind(referred)
    .bind(format!("privacy-acq-{referred}"))
    .bind(code)
    .bind(child)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_provenance_events(
             workspace_id,fan_id,event_kind,channel,attribution_method,occurred_at
         ) VALUES($1,$2,'conversion','referral','last_tracked_click',now())",
    )
    .bind(w)
    .bind(child)
    .execute(&pool)
    .await?;

    let token = format!("privacy-session-{}", Uuid::now_v7());
    sqlx::query(
        "INSERT INTO fan_sessions(
             workspace_id,fan_id,session_token_hash,expires_at
         ) VALUES($1,$2,digest($3,'sha256'),now()+interval '1 day')",
    )
    .bind(w)
    .bind(child)
    .bind(&token)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO outbox_events(workspace_id,event_type,payload)
         VALUES(
             $1,'fan.lifecycle.message_requested',
             jsonb_build_object(
                 'fan_id',$2::uuid::text,
                 'email','privacy-child@example.test'
             )
         )",
    )
    .bind(w)
    .bind(child)
    .execute(&pool)
    .await?;

    let identity = PgFanIdentityRepository::new(pool.clone());
    identity
        .merge_fans(&MergeFansCommand {
            workspace_id: workspace,
            survivor_fan_id: survivor,
            merged_fan_id: child,
            reason: Some("same person".to_owned()),
            merged_by: "privacy-test".to_owned(),
            request_id: "privacy-family-merge".to_owned(),
        })
        .await?;

    let code_state: (Uuid, bool) = sqlx::query_as(
        "SELECT fan_id,active FROM referral_codes
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(w)
    .bind(code)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        code_state,
        (child, true),
        "referenced code must be pinned to the historical row before erasure"
    );

    let privacy = PostgresFanPrivacyRepository::new(pool.clone());
    let receipt = privacy
        .erase_account(w, &token, Some("privacy-family-erasure"))
        .await
        .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
    assert_eq!(receipt.fan_id, survivor);

    let rows: Vec<(Uuid, String, String, Option<time::OffsetDateTime>)> = sqlx::query_as(
        "SELECT id,normalized_email,status,deleted_at
         FROM fans
         WHERE workspace_id=$1 AND id IN ($2,$3)
         ORDER BY id",
    )
    .bind(w)
    .bind(survivor)
    .bind(child)
    .fetch_all(&pool)
    .await?;
    assert_eq!(rows.len(), 2);
    for (id, email, status, deleted_at) in rows {
        assert!(email.ends_with("@account.invalid"), "{id}: {email}");
        assert!(deleted_at.is_some(), "{id} was not privacy tombstoned");
        if id == survivor {
            assert_eq!(status, "suppressed");
        } else {
            assert_eq!(status, "merged");
        }
    }

    let code_active: bool = sqlx::query_scalar(
        "SELECT active FROM referral_codes WHERE workspace_id=$1 AND id=$2",
    )
    .bind(w)
    .bind(code)
    .fetch_one(&pool)
    .await?;
    assert!(!code_active);

    let graph: (i64, i64, i64) = sqlx::query_as(
        "SELECT
           (SELECT count(*) FROM referral_attributions
             WHERE workspace_id=$1
               AND (referrer_fan_id IN (
                      SELECT fan_id FROM canonical_fan_family($1,$2)
                   ) OR referred_fan_id IN (
                      SELECT fan_id FROM canonical_fan_family($1,$2)
                   )))::bigint,
           (SELECT count(*) FROM fan_acquisition_events
             WHERE workspace_id=$1
               AND (fan_id IN (
                      SELECT fan_id FROM canonical_fan_family($1,$2)
                   ) OR referrer_fan_id IN (
                      SELECT fan_id FROM canonical_fan_family($1,$2)
                   )))::bigint,
           (SELECT count(*) FROM fan_provenance_events
             WHERE workspace_id=$1
               AND fan_id IN (
                   SELECT fan_id FROM canonical_fan_family($1,$2)
               ))::bigint",
    )
    .bind(w)
    .bind(survivor)
    .fetch_one(&pool)
    .await?;
    assert_eq!(graph, (0, 0, 0));

    let outbox: (String, serde_json::Value) = sqlx::query_as(
        "SELECT status,payload FROM outbox_events
         WHERE workspace_id=$1
           AND event_type='fan.lifecycle.message_requested'
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(w)
    .fetch_one(&pool)
    .await?;
    assert_eq!(outbox.0, "dead");
    assert_eq!(outbox.1["identity_erased"], true);
    assert_eq!(outbox.1["fan_id"], survivor.to_string());

    let identifiers: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fan_identifiers
         WHERE workspace_id=$1
           AND fan_id IN (SELECT fan_id FROM canonical_fan_family($1,$2))",
    )
    .bind(w)
    .bind(survivor)
    .fetch_one(&pool)
    .await?;
    assert_eq!(identifiers, 0);

    let consent_evidence: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fan_consents
         WHERE workspace_id=$1
           AND fan_id IN (SELECT fan_id FROM canonical_fan_family($1,$2))",
    )
    .bind(w)
    .bind(survivor)
    .fetch_one(&pool)
    .await?;
    assert!(
        consent_evidence >= 1,
        "privacy erasure keeps consent evidence while removing identity surfaces"
    );
    Ok(())
}
