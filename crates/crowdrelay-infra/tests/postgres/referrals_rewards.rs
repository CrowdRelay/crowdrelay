use std::time::Duration;

use crate::common;
use crowdrelay_application::{
    AcquisitionRepository, ConfirmFanCommand, FanIdentityRepository, FanLifecycleRepository,
    IdempotencyKey, MergeFansCommand, RedeemCouponCommand, ReferralRepository, RepositoryError,
    RequestId, SignupFanCommand,
};
use crowdrelay_domain::{
    CitySlug, CountryCode, FanActionToken, FanSignup, FanSignupInput, MarketingConsent,
    NormalizedEmail, PhysicalRewardStatus, VisitorId, WorkspaceId, WorkspaceSlug,
};
use crowdrelay_infra::{
    acquisition::PostgresAcquisitionRepository,
    config::DatabaseConfig,
    fan_identity::PgFanIdentityRepository,
    fan_lifecycle::PostgresFanLifecycleRepository,
    referrals::{PostgresReferralRepository, record_referral_interaction},
    sensitive_response::{SensitiveResponseCodec, SensitiveResponseKey},
};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires CROWDRELAY_REFERRAL_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn qualifies_referrals_grants_one_coupon_and_redeems_idempotently()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_REFERRAL_TEST_DATABASE_URL").await?;
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

    let visitor_id = VisitorId::new();
    let referral_code = referrer.referral_code.as_ref().ok_or("referrer code")?;
    assert!(
        record_referral_interaction(
            &pool,
            workspace_id,
            referral_code,
            visitor_id,
            time::OffsetDateTime::now_utc(),
        )
        .await?
    );
    let interaction: (Option<Uuid>, String, String, Option<Uuid>, String) = sqlx::query_as(
        r#"SELECT fan_id, channel, source_target, anonymous_visitor_id, attribution_method
           FROM fan_provenance_events
           WHERE workspace_id=$1 AND anonymous_visitor_id=$2 AND event_kind='interaction'"#,
    )
    .bind(workspace_id.into_uuid())
    .bind(Into::<Uuid>::into(visitor_id))
    .fetch_one(&pool)
    .await?;
    assert_eq!(interaction.0, None, "click stays anonymous until signup");
    assert_eq!(interaction.1, "referral");
    assert_eq!(
        interaction.2,
        format!("fan:{}", referrer.fan_id.into_uuid())
    );
    assert_eq!(interaction.3, Some(Into::<Uuid>::into(visitor_id)));
    assert_eq!(interaction.4, "referral_click");

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

    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_REFERRAL_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn pinned_referral_code_stays_live_and_rewards_the_canonical_survivor_after_merge()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_REFERRAL_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let workspace_slug = WorkspaceSlug::parse(format!(
        "referral-merge-{}",
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

    let historical = acquisition
        .persist_fan_signup(&signup_command(
            workspace_id,
            "historical-referrer@example.test",
            None,
            "merge-historical-referrer",
        )?)
        .await?;
    let old_code = historical.referral_code.clone().ok_or("historical code")?;
    let old_session = historical
        .fan_session_token
        .clone()
        .ok_or("historical session")?;

    // This first real conversion pins the old code owner through the composite
    // attribution FK. Merge must preserve that exact history.
    acquisition
        .persist_fan_signup(&signup_command(
            workspace_id,
            "before-merge@example.test",
            Some(old_code.clone()),
            "merge-referred-before",
        )?)
        .await?;

    let canonical = acquisition
        .persist_fan_signup(&signup_command(
            workspace_id,
            "canonical-referrer@example.test",
            None,
            "merge-canonical-referrer",
        )?)
        .await?;
    let identity = PgFanIdentityRepository::new(pool.clone());
    identity
        .merge_fans(&MergeFansCommand {
            workspace_id,
            survivor_fan_id: canonical.fan_id.into_uuid(),
            merged_fan_id: historical.fan_id.into_uuid(),
            reason: Some("same person".to_owned()),
            merged_by: "referral-merge-test".to_owned(),
            request_id: "referral-code-owner-merge".to_owned(),
        })
        .await?;

    let code_owner: (Uuid, String, Option<Uuid>) = sqlx::query_as(
        "SELECT code.fan_id, fan.status, fan.merged_into_fan_id
         FROM referral_codes code
         JOIN fans fan ON fan.workspace_id=code.workspace_id AND fan.id=code.fan_id
         WHERE code.workspace_id=$1 AND code.code=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(old_code.as_str())
    .fetch_one(&pool)
    .await?;
    assert_eq!(code_owner.0, historical.fan_id.into_uuid());
    assert_eq!(code_owner.1, "merged");
    assert_eq!(code_owner.2, Some(canonical.fan_id.into_uuid()));
    assert!(
        referrals
            .referral_code_is_active(workspace_id, &old_code)
            .await?,
        "a shared code pinned to history must remain operational through its canonical owner"
    );

    // Two more independent humans use the exact same already-shared code after
    // the owner merge. Together with the pre-merge conversion this reaches the
    // threshold of three.
    for (index, email) in [
        (1, "after-merge-one@example.test"),
        (2, "after-merge-two@example.test"),
    ] {
        acquisition
            .persist_fan_signup(&signup_command(
                workspace_id,
                email,
                Some(old_code.clone()),
                &format!("merge-referred-after-{index}"),
            )?)
            .await?;
    }

    let progress = referrals
        .load_referral_progress(workspace_id, &old_session)
        .await?;
    assert_eq!(progress.qualified_referrals, 3);
    assert_eq!(progress.pending_referrals, 0);
    assert_eq!(
        progress.referral_code, old_code,
        "the already-distributed code remains the fan-visible stable code"
    );
    assert_eq!(progress.coupons.len(), 1);

    let grants: Vec<Uuid> = sqlx::query_scalar(
        "SELECT fan_id FROM reward_grants
         WHERE workspace_id=$1 AND status='issued'
         ORDER BY created_at,id",
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        grants,
        vec![canonical.fan_id.into_uuid()],
        "reward ownership must be canonical even though attribution history stays pinned"
    );

    let attributed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM referral_attributions
         WHERE workspace_id=$1
           AND referrer_fan_id=$2
           AND status='qualified'",
    )
    .bind(workspace_id.into_uuid())
    .bind(historical.fan_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        attributed, 3,
        "all attribution rows keep the exact historical code owner for audit"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_REFERRAL_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn merging_duplicate_referred_people_revokes_ambiguous_double_credit_and_unmerge_restores_it()
-> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_application::UnmergeFanCommand;

    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_REFERRAL_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let workspace_slug = WorkspaceSlug::parse(format!(
        "referral-ambiguity-{}",
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
    let identity = PgFanIdentityRepository::new(pool.clone());

    let referrer_a = acquisition
        .persist_fan_signup(&signup_command(
            workspace_id,
            "ambiguity-referrer-a@example.test",
            None,
            "ambiguity-referrer-a",
        )?)
        .await?;
    let referrer_b = acquisition
        .persist_fan_signup(&signup_command(
            workspace_id,
            "ambiguity-referrer-b@example.test",
            None,
            "ambiguity-referrer-b",
        )?)
        .await?;
    let code_a = referrer_a.referral_code.clone().ok_or("code A")?;
    let code_b = referrer_b.referral_code.clone().ok_or("code B")?;

    for (index, code, prefix) in [
        (1, code_a.clone(), "a"),
        (2, code_a.clone(), "a"),
        (1, code_b.clone(), "b"),
        (2, code_b.clone(), "b"),
    ] {
        acquisition
            .persist_fan_signup(&signup_command(
                workspace_id,
                &format!("ambiguity-{prefix}-{index}@example.test"),
                Some(code),
                &format!("ambiguity-{prefix}-{index}"),
            )?)
            .await?;
    }

    // These look like two different third people before identity resolution,
    // so each referrer legitimately reaches threshold 3 at this point.
    let duplicate_a = acquisition
        .persist_fan_signup(&signup_command(
            workspace_id,
            "ambiguity-duplicate-a@example.test",
            Some(code_a),
            "ambiguity-duplicate-a",
        )?)
        .await?;
    let duplicate_b = acquisition
        .persist_fan_signup(&signup_command(
            workspace_id,
            "ambiguity-duplicate-b@example.test",
            Some(code_b),
            "ambiguity-duplicate-b",
        )?)
        .await?;

    for referrer in [&referrer_a, &referrer_b] {
        let progress = referrals
            .load_referral_progress(
                workspace_id,
                referrer.fan_session_token.as_ref().ok_or("session")?,
            )
            .await?;
        assert_eq!(progress.qualified_referrals, 3);
        assert_eq!(progress.coupons.len(), 1);
    }

    identity
        .merge_fans(&MergeFansCommand {
            workspace_id,
            survivor_fan_id: duplicate_a.fan_id.into_uuid(),
            merged_fan_id: duplicate_b.fan_id.into_uuid(),
            reason: Some("same referred person".to_owned()),
            merged_by: "referral-ambiguity-test".to_owned(),
            request_id: "merge-ambiguous-referred".to_owned(),
        })
        .await?;

    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT canonical_qualified_referral_owner_id($1,$2)")
            .bind(workspace_id.into_uuid())
            .bind(duplicate_a.fan_id.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        owner, None,
        "two distinct canonical referrers for one human are ambiguous, never timestamp-resolved"
    );

    for referrer in [&referrer_a, &referrer_b] {
        let progress = referrals
            .load_referral_progress(
                workspace_id,
                referrer.fan_session_token.as_ref().ok_or("session")?,
            )
            .await?;
        assert_eq!(
            progress.qualified_referrals, 2,
            "ambiguous person must credit neither referrer"
        );
        let issued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM reward_grants
             WHERE workspace_id=$1 AND fan_id=$2 AND status='issued'",
        )
        .bind(workspace_id.into_uuid())
        .bind(referrer.fan_id.into_uuid())
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            issued, 0,
            "merge reconciliation must revoke unearned issued reward"
        );
        let revoked_coupon: i64 = sqlx::query_scalar(
            "SELECT count(*)
             FROM merch_coupons coupon
             JOIN reward_grants grant_row
               ON grant_row.workspace_id=coupon.workspace_id
              AND grant_row.id=coupon.reward_grant_id
             WHERE coupon.workspace_id=$1
               AND grant_row.fan_id=$2
               AND coupon.status='revoked'",
        )
        .bind(workspace_id.into_uuid())
        .bind(referrer.fan_id.into_uuid())
        .fetch_one(&pool)
        .await?;
        assert_eq!(revoked_coupon, 1);
    }

    identity
        .unmerge_fan(&UnmergeFanCommand {
            workspace_id,
            merged_fan_id: duplicate_b.fan_id.into_uuid(),
            unmerged_by: "referral-ambiguity-test".to_owned(),
            request_id: "unmerge-ambiguous-referred".to_owned(),
        })
        .await?;

    for referrer in [&referrer_a, &referrer_b] {
        let progress = referrals
            .load_referral_progress(
                workspace_id,
                referrer.fan_session_token.as_ref().ok_or("session")?,
            )
            .await?;
        assert_eq!(progress.qualified_referrals, 3);
        let issued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM reward_grants
             WHERE workspace_id=$1 AND fan_id=$2 AND status='issued'",
        )
        .bind(workspace_id.into_uuid())
        .bind(referrer.fan_id.into_uuid())
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            issued, 1,
            "unmerge reconciliation may reactivate the same entitlement exactly once"
        );
    }
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
        city_slug: Some(CitySlug::parse("wroclaw")?),
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
