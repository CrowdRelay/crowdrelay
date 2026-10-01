use super::*;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::config::AdConversionConfig;
use sqlx::PgPool;
use std::time::Duration;

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn ad_conversions_require_current_consent_and_active_recipient()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = PgPool::connect(&std::env::var("CROWDRELAY_TEST_DATABASE_URL")?).await?;
    let workspace = WorkspaceId::new();
    let fan = Uuid::now_v7();
    let tenant = workspace.into_uuid();
    let email = format!("{fan}@example.test");
    sqlx::query("INSERT INTO workspaces (id,slug,name) VALUES ($1,$2,'Ad consent proof')")
        .bind(tenant)
        .bind(format!("ad-consent-{}", tenant.simple()))
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO fans (id,workspace_id,normalized_email,status) VALUES ($1,$2,$3,'active')",
    )
    .bind(fan)
    .bind(tenant)
    .bind(&email)
    .execute(&pool)
    .await?;
    sqlx::query("INSERT INTO fan_consents (workspace_id,fan_id,purpose,granted,policy_version,source,recorded_at) VALUES ($1,$2,'marketing',true,'v1','proof',now()-interval '1 day')")
        .bind(tenant).bind(fan).execute(&pool).await?;
    sqlx::query("INSERT INTO fan_ad_attribution (workspace_id,fan_id,meta_fbp) VALUES ($1,$2,'fb.1.1700000000000.1234567890')")
        .bind(tenant).bind(fan).execute(&pool).await?;
    let event: Uuid = sqlx::query_scalar("INSERT INTO events (workspace_id,slug,title,starts_at,status) VALUES ($1,$2,'Proof',now()+interval '10 days','draft') RETURNING id")
        .bind(tenant).bind(format!("event-{}", fan.simple())).fetch_one(&pool).await?;
    let admission: Uuid = sqlx::query_scalar("INSERT INTO admission_pools (workspace_id,event_id,name,slug,capacity) VALUES ($1,$2,'Proof','proof',10) RETURNING id")
        .bind(tenant).bind(event).fetch_one(&pool).await?;
    let sale: Uuid = sqlx::query_scalar("INSERT INTO ticket_sales (workspace_id,event_id,admission_pool_id,capacity,sales_open_at,sales_close_at) VALUES ($1,$2,$3,10,now()-interval '1 day',now()+interval '5 days') RETURNING id")
        .bind(tenant).bind(event).bind(admission).fetch_one(&pool).await?;
    sqlx::query("INSERT INTO ticket_orders (workspace_id,ticket_sale_id,public_reference,status,buyer_email,currency,amount_gross_minor,amount_net_minor,amount_vat_minor,vat_rate_basis_points,reservation_key,request_hash,checkout_token_hash,expires_at,paid_at) VALUES ($1,$2,'VRY-ORD-ABCDEF0123456789','paid',$3,'PLN',100,100,0,0,'proof',digest('proof','sha256'),digest('checkout','sha256'),now()+interval '1 day',now())")
        .bind(tenant).bind(sale).bind(&email).execute(&pool).await?;
    let worker = AdConversionWorker::new(
        pool.clone(),
        workspace,
        AdConversionConfig::default(),
        Duration::from_secs(5),
    )?;
    assert_eq!(worker.fetch_pending_fans("meta", "Lead").await?.len(), 1);
    assert_eq!(
        worker.fetch_pending_orders("meta", "Purchase").await?.len(),
        1
    );
    worker.ensure_recipient_eligible(Some(fan)).await?;
    sqlx::query("INSERT INTO fan_consents (workspace_id,fan_id,purpose,granted,policy_version,source) VALUES ($1,$2,'marketing',false,'v1','proof')")
        .bind(tenant).bind(fan).execute(&pool).await?;
    assert!(worker.fetch_pending_fans("meta", "Lead").await?.is_empty());
    assert!(
        worker
            .fetch_pending_orders("meta", "Purchase")
            .await?
            .is_empty()
    );
    assert!(matches!(
        worker.ensure_recipient_eligible(Some(fan)).await,
        Err(AdConversionError::Ineligible)
    ));
    sqlx::query("UPDATE fans SET status='unsubscribed' WHERE workspace_id=$1 AND id=$2")
        .bind(tenant)
        .bind(fan)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO fan_consents (workspace_id,fan_id,purpose,granted,policy_version,source) VALUES ($1,$2,'marketing',true,'v1','proof')")
        .bind(tenant).bind(fan).execute(&pool).await?;
    assert!(
        worker
            .fetch_pending_orders("meta", "Purchase")
            .await?
            .is_empty()
    );
    assert!(matches!(
        worker.ensure_recipient_eligible(None).await,
        Err(AdConversionError::Ineligible)
    ));
    assert!(matches!(
        worker.ensure_recipient_eligible(Some(Uuid::now_v7())).await,
        Err(AdConversionError::Ineligible)
    ));
    Ok(())
}
