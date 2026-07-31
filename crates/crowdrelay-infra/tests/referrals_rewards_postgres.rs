use std::time::Duration;

use crowdrelay_application::{
    AcquisitionRepository, ConfirmFanCommand, FanLifecycleRepository, IdempotencyKey,
    RedeemCouponCommand, ReferralRepository, RepositoryError, RequestId, SignupFanCommand,
};
use crowdrelay_domain::{
    CitySlug, CountryCode, FanActionToken, FanSignup, FanSignupInput, MarketingConsent,
    NormalizedEmail, PhysicalRewardStatus, WorkspaceId, WorkspaceSlug,
};
use crowdrelay_infra::{
    acquisition::PostgresAcquisitionRepository,
    config::DatabaseConfig,
    fan_lifecycle::PostgresFanLifecycleRepository,
    referrals::PostgresReferralRepository,
    sensitive_response::{SensitiveResponseCodec, SensitiveResponseKey},
};
use serde_json::Value;
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires CROWDRELAY_REFERRAL_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn qualifies_referrals_grants_one_coupon_and_redeems_idempotently()
-> Result<(), Box<dyn std::error::Error>> {
    let database_url = std::env::var("CROWDRELAY_REFERRAL_TEST_DATABASE_URL").map_err(|e| {
        format!("CROWDRELAY_REFERRAL_TEST_DATABASE_URL must target a disposable database: {e}")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

    let workspace_id = WorkspaceId::new();
    let workspace_slug = WorkspaceSlug::parse(format!(
        "referral-e2e-{}",
        workspace_id.into_uuid().simple()
    ))?;
    seed_fixture(&pool, workspace_id, &workspace_slug).await?;

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 8,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
    };
    let acquisition = PostgresAcquisitionRepository::new(
        pool.clone(),
        workspace_slug.clone(),
        CountryCode::parse("PL")?,
        &database,
        false,
        test_sensitive_response_codec(),
    );
    let referrals =
        PostgresReferralRepository::new(pool.clone(), workspace_slug.clone(), &database);
    let lifecycle = PostgresFanLifecycleRepository::new(
        pool.clone(),
        workspace_slug.clone(),
        &database,
        test_sensitive_response_codec(),
    );

    let referrer = acquisition
        .persist_fan_signup(&signup_command(
            workspace_id,
            "referrer@example.test",
            None,
            "signup-referrer-0001",
        )?)
        .await?;

    for index in 1..=3 {
        acquisition
            .persist_fan_signup(&signup_command(
                workspace_id,
                &format!("referred-{index}@example.test"),
                referrer.referral_code.clone(),
                &format!("signup-referred-{index:04}"),
            )?)
            .await?;
    }

    let pending_acquisition = PostgresAcquisitionRepository::new(
        pool.clone(),
        workspace_slug,
        CountryCode::parse("PL")?,
        &database,
        true,
        test_sensitive_response_codec(),
    );
    let pending = pending_acquisition
        .persist_fan_signup(&signup_command(
            workspace_id,
            "pending-referred@example.test",
            referrer.referral_code.clone(),
            "signup-pending-referred",
        )?)
        .await?;
    let progress_with_pending = referrals
        .load_referral_progress(
            workspace_id,
            referrer
                .fan_session_token
                .as_ref()
                .ok_or("active fan session")?,
        )
        .await?;
    assert_eq!(progress_with_pending.qualified_referrals, 3);
    assert_eq!(progress_with_pending.pending_referrals, 1);

    let confirmation_token = outbox_token_for_fan(
        &pool,
        workspace_id,
        pending.fan_id.into_uuid(),
        "fan.confirmation_requested",
        "confirmation_token",
    )
    .await?;
    lifecycle
        .confirm(&ConfirmFanCommand {
            workspace_id,
            token: FanActionToken::parse(confirmation_token)?,
            idempotency_key: IdempotencyKey::parse("confirm-pending-referred")?,
            request_id: RequestId::parse("request-confirm-pending-referred")?,
        })
        .await?;
    let confirmed_progress = referrals
        .load_referral_progress(
            workspace_id,
            referrer
                .fan_session_token
                .as_ref()
                .ok_or("active fan session")?,
        )
        .await?;
    assert_eq!(confirmed_progress.qualified_referrals, 4);
    assert_eq!(confirmed_progress.pending_referrals, 0);
    assert_eq!(confirmed_progress.physical_rewards.len(), 1);
    assert_eq!(
        confirmed_progress.physical_rewards[0].status,
        PhysicalRewardStatus::Issued
    );
    let physical_grant_id = confirmed_progress.physical_rewards[0].reward_grant_id;

    let unsubscribe_token = outbox_token_for_fan(
        &pool,
        workspace_id,
        pending.fan_id.into_uuid(),
        "fan.confirmed",
        "unsubscribe_token",
    )
    .await?;
    lifecycle
        .unsubscribe(workspace_id, &FanActionToken::parse(unsubscribe_token)?)
        .await?;
    let progress = referrals
        .load_referral_progress(
            workspace_id,
            referrer
                .fan_session_token
                .as_ref()
                .ok_or("active fan session")?,
        )
        .await?;
    assert_eq!(progress.qualified_referrals, 3);
    assert_eq!(progress.pending_referrals, 0);
    assert_eq!(progress.coupons.len(), 1);
    assert_eq!(progress.physical_rewards.len(), 1);
    assert_eq!(
        progress.physical_rewards[0].status,
        PhysicalRewardStatus::Revoked
    );
    acquisition
        .persist_fan_signup(&signup_command(
            workspace_id,
            "replacement-referred@example.test",
            referrer.referral_code.clone(),
            "signup-replacement-referred",
        )?)
        .await?;
    let requalified_progress = referrals
        .load_referral_progress(
            workspace_id,
            referrer
                .fan_session_token
                .as_ref()
                .ok_or("active fan session")?,
        )
        .await?;
    assert_eq!(requalified_progress.qualified_referrals, 4);
    assert_eq!(requalified_progress.physical_rewards.len(), 1);
    assert_eq!(
        requalified_progress.physical_rewards[0].reward_grant_id, physical_grant_id,
        "requalification must reactivate the accounting record rather than duplicate it"
    );
    assert_eq!(
        requalified_progress.physical_rewards[0].status,
        PhysicalRewardStatus::Issued
    );
    let physical_granted_event_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM outbox_events \
         WHERE workspace_id = $1 AND event_type = 'physical_reward.granted'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(physical_granted_event_count, 2);
    let coupon = progress.coupons[0].clone();
    assert_eq!(coupon.discount_percent, 10.0);
    assert_eq!(coupon.used_count, 0);

    let redemption = RedeemCouponCommand::new(
        workspace_id,
        IdempotencyKey::parse("redeem-order-0001")?,
        RequestId::parse("request-redeem-0001")?,
        coupon.code.clone(),
        "order-e2e-0001",
    )?;
    let first = referrals.redeem_coupon(&redemption).await?;
    let replay_command = RedeemCouponCommand::new(
        workspace_id,
        IdempotencyKey::parse("redeem-order-0001")?,
        RequestId::parse("request-redeem-0001-retry")?,
        coupon.code.clone(),
        "order-e2e-0001",
    )?;
    let replay = referrals.redeem_coupon(&replay_command).await?;
    assert_eq!(first, replay);
    assert_eq!(first.used_count, 1);

    let second_key = RedeemCouponCommand::new(
        workspace_id,
        IdempotencyKey::parse("redeem-order-0002")?,
        RequestId::parse("request-redeem-0002")?,
        coupon.code.clone(),
        "order-e2e-0002",
    )?;
    assert_eq!(
        referrals.redeem_coupon(&second_key).await,
        Err(RepositoryError::Conflict),
        "a one-time coupon cannot be consumed by a new checkout request"
    );

    let redemption_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM coupon_redemptions WHERE workspace_id = $1",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(redemption_count, 1);

    let issued_event_count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)::bigint
        FROM outbox_events
        WHERE workspace_id = $1 AND event_type = 'merch_coupon.issued'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(issued_event_count, 1);

    let redeemed_event_count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)::bigint
        FROM outbox_events
        WHERE workspace_id = $1 AND event_type = 'merch_coupon.redeemed'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(redeemed_event_count, 1);

    pool.close().await;
    Ok(())
}

async fn outbox_token_for_fan(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    fan_id: Uuid,
    event_type: &str,
    field: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let payload = sqlx::query_scalar::<_, Value>(
        r#"
        SELECT payload
        FROM outbox_events
        WHERE workspace_id = $1
            AND event_type = $2
            AND payload ->> 'fan_id' = $3::text
        ORDER BY created_at DESC, id DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_type)
    .bind(fan_id)
    .fetch_one(pool)
    .await?;
    payload[field]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{event_type}.{field} must be a string").into())
}

fn signup_command(
    workspace_id: WorkspaceId,
    email: &str,
    referral_code: Option<crowdrelay_domain::ReferralCode>,
    key: &str,
) -> Result<SignupFanCommand, Box<dyn std::error::Error>> {
    let signup = FanSignup::new(FanSignupInput {
        workspace_id,
        email: NormalizedEmail::parse(email)?,
        display_name: None,
        city_slug: CitySlug::parse("wroclaw")?,
        locale: Some("pl".to_owned()),
        campaign_id: None,
        visitor_id: None,
        claimed_referral_code: referral_code,
        consent: MarketingConsent::new(true, "privacy-2026-07", "integration_test")?,
    })?;
    Ok(SignupFanCommand::new(
        IdempotencyKey::parse(key)?,
        RequestId::parse(format!("request-{key}"))?,
        signup,
    ))
}

async fn seed_fixture(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    slug: &WorkspaceSlug,
) -> Result<(), Box<dyn std::error::Error>> {
    let city_id = Uuid::now_v7();
    let mut transaction = pool.begin().await?;
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(slug.as_str())
        .bind("Referral rewards E2E")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        r#"
        INSERT INTO cities (id, slug, name, country_code)
        VALUES ($1, 'wroclaw', 'Wrocław', 'PL')
        ON CONFLICT (country_code, slug) DO UPDATE SET name = EXCLUDED.name
        "#,
    )
    .bind(city_id)
    .execute(&mut *transaction)
    .await?;
    let city_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM cities WHERE country_code = 'PL' AND slug = 'wroclaw'",
    )
    .fetch_one(&mut *transaction)
    .await?;
    sqlx::query("INSERT INTO city_aggregates (workspace_id, city_id) VALUES ($1, $2)")
        .bind(workspace_id.into_uuid())
        .bind(city_id)
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        r#"
        INSERT INTO reward_rules (
            workspace_id, name, reward_type, threshold, config, active, version
        )
        VALUES
            (
                $1,
                '3 qualified fans = 10% merch',
                'merch_discount',
                3,
                '{"discount_percent":10.0,"expires_days":30,"code_prefix":"VIRYA"}',
                true,
                1
            ),
            (
                $1,
                '4 qualified fans = physical album',
                'physical_item',
                4,
                '{"item_name":"Virya album","sku":"VIRYA-CD","expires_days":90}',
                true,
                1
            )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

fn test_sensitive_response_codec() -> SensitiveResponseCodec {
    SensitiveResponseCodec::new(SensitiveResponseKey::derive_from_secret(
        b"referrals-integration-response-secret",
    ))
}
