//! Read the migrated schema, not a mock shape: orders own a sale, sales own a show.
use super::install_tests::{consented_fan, install_fixture};
use super::*;

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn lifecycle_evidence_counts_paid_shows_and_only_qualified_referrals()
-> Result<(), Box<dyn std::error::Error>> {
    let f = install_fixture("lifecycle-evidence").await?;
    let fan = consented_fan(&f).await?;
    let referred = consented_fan(&f).await?;
    let code = Uuid::now_v7();
    sqlx::query("INSERT INTO referral_codes(id,workspace_id,fan_id,code) VALUES($1,$2,$3,$4)")
        .bind(code)
        .bind(f.workspace_id.into_uuid())
        .bind(fan.into_uuid())
        .bind(format!("episode-{}", code.simple()))
        .execute(&f.pool)
        .await?;
    sqlx::query("INSERT INTO referral_attributions(workspace_id,referrer_fan_id,referred_fan_id,referral_code_id,status,accepted_at) VALUES($1,$2,$3,$4,'pending',now()-interval '2 days')")
        .bind(f.workspace_id.into_uuid()).bind(fan.into_uuid()).bind(referred.into_uuid()).bind(code).execute(&f.pool).await?;
    let first_sale = paid_show(&f, "first").await?;
    let second_sale = paid_show(&f, "second").await?;
    for _ in 0..5 {
        paid_order(
            &f,
            fan,
            first_sale,
            OffsetDateTime::now_utc() - time::Duration::hours(2),
        )
        .await?;
    }
    paid_order(
        &f,
        fan,
        second_sale,
        OffsetDateTime::now_utc() + time::Duration::days(1),
    )
    .await?;
    let now = OffsetDateTime::now_utc();
    let snapshots = f
        .repository
        .load_fan_lifecycle_snapshots(f.workspace_id, now)
        .await?;
    let snapshot = snapshots
        .iter()
        .find(|s| s.fan_id == fan)
        .ok_or("fan snapshot")?;
    assert_eq!(
        snapshot.paid_ticket_count, 1,
        "five orders for one show are one visit"
    );
    assert!(snapshot.has_paid_ticket);
    assert_eq!(snapshot.qualified_referrals, 0);
    assert_eq!(snapshot.last_qualified_referral_at, None);
    let qualified_at = now - time::Duration::hours(1);
    sqlx::query("UPDATE referral_attributions SET status='qualified',qualified_at=$3 WHERE workspace_id=$1 AND referrer_fan_id=$2")
        .bind(f.workspace_id.into_uuid()).bind(fan.into_uuid()).bind(qualified_at).execute(&f.pool).await?;
    paid_order(&f, fan, second_sale, now - time::Duration::minutes(30)).await?;
    let snapshots = f
        .repository
        .load_fan_lifecycle_snapshots(f.workspace_id, now)
        .await?;
    let snapshot = snapshots
        .iter()
        .find(|s| s.fan_id == fan)
        .ok_or("fan snapshot")?;
    assert_eq!(snapshot.paid_ticket_count, 2);
    assert_eq!(snapshot.qualified_referrals, 1);
    // PostgreSQL stores microseconds, so use the exact value read back.
    let stored: OffsetDateTime = sqlx::query_scalar("SELECT qualified_at FROM referral_attributions WHERE workspace_id=$1 AND referrer_fan_id=$2")
        .bind(f.workspace_id.into_uuid()).bind(fan.into_uuid()).fetch_one(&f.pool).await?;
    assert_eq!(snapshot.last_qualified_referral_at, Some(stored));
    assert_ne!(
        snapshot.last_qualified_referral_at,
        Some(now - time::Duration::days(2))
    );
    sqlx::query("UPDATE referral_attributions SET status='reversed',reversed_at=$3 WHERE workspace_id=$1 AND referrer_fan_id=$2")
        .bind(f.workspace_id.into_uuid()).bind(fan.into_uuid()).bind(now).execute(&f.pool).await?;
    let snapshots = f
        .repository
        .load_fan_lifecycle_snapshots(f.workspace_id, now)
        .await?;
    let snapshot = snapshots
        .iter()
        .find(|s| s.fan_id == fan)
        .ok_or("fan snapshot")?;
    assert_eq!(snapshot.qualified_referrals, 0);
    assert_eq!(snapshot.last_qualified_referral_at, None);
    Ok(())
}

async fn paid_show(f: &Fixture, label: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    let event = Uuid::now_v7();
    let pool = Uuid::now_v7();
    let sale = Uuid::now_v7();
    sqlx::query("INSERT INTO events(id,workspace_id,slug,title,starts_at,status) VALUES($1,$2,$3,'Lifecycle show',now()+interval '7 days','draft')")
        .bind(event).bind(f.workspace_id.into_uuid()).bind(label).execute(&f.pool).await?;
    sqlx::query("INSERT INTO admission_pools(id,workspace_id,event_id,name,slug,capacity) VALUES($1,$2,$3,'Tickets','tickets',100)")
        .bind(pool).bind(f.workspace_id.into_uuid()).bind(event).execute(&f.pool).await?;
    sqlx::query("INSERT INTO ticket_sales(id,workspace_id,event_id,admission_pool_id,capacity,sales_open_at,sales_close_at) VALUES($1,$2,$3,$4,100,now()-interval '1 day',now()+interval '7 days')")
        .bind(sale).bind(f.workspace_id.into_uuid()).bind(event).bind(pool).execute(&f.pool).await?;
    Ok(sale)
}

async fn paid_order(
    f: &Fixture,
    fan: crowdrelay_domain::FanId,
    sale: Uuid,
    paid_at: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO ticket_orders(id,workspace_id,ticket_sale_id,public_reference,status,buyer_email,currency,amount_gross_minor,amount_net_minor,amount_vat_minor,vat_rate_basis_points,reservation_key,request_hash,checkout_token_hash,expires_at,paid_at) VALUES($1,$2,$3,$4,'paid',$5,'PLN',1080,1000,80,800,$6,decode(repeat('00',32),'hex'),decode(repeat('11',32),'hex'),now()+interval '2 days',$7)")
        .bind(id).bind(f.workspace_id.into_uuid()).bind(sale)
        .bind(format!("VRY-ORD-{}",id.simple().to_string().chars().skip(16).collect::<String>().to_uppercase()))
        .bind(format!("{fan}@example.test")).bind(id.to_string()).bind(paid_at)
        .execute(&f.pool).await?;
    Ok(())
}
