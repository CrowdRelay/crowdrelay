use std::time::Duration;

use crowdrelay_application::{
    AcquisitionRepository, IdempotencyKey, RepositoryError, RequestId, SignupFanCommand,
};
use crowdrelay_domain::{
    CampaignId, CitySlug, ClickEvent, CountryCode, DestinationUrl, FanSignup, FanSignupInput,
    FanStatus, MarketingConsent, NormalizedEmail, ReferralCode, ResolvedSmartLink, SmartLinkId,
    SmartLinkSlug, VisitorId, WorkspaceId, WorkspaceSlug,
};
use crowdrelay_infra::{
    acquisition::PostgresAcquisitionRepository,
    config::DatabaseConfig,
    database,
    sensitive_response::{SensitiveResponseCodec, SensitiveResponseKey},
};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

const TEST_DATABASE_URL_KEY: &str = "CROWDRELAY_TEST_DATABASE_URL";

#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn phase_one_acquisition_is_atomic_and_tenant_safe() -> Result<(), Box<dyn std::error::Error>>
{
    let database_url = std::env::var(TEST_DATABASE_URL_KEY)
        .map_err(|e| format!("set CROWDRELAY_TEST_DATABASE_URL: {e}"))?;
    let database_config = DatabaseConfig {
        url: database_url,
        max_connections: 8,
        connect_timeout: Duration::from_secs(5),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(2),
    };
    let pool = database::connect(&database_config).await?;
    database::migrate(&pool).await?;

    let suffix = Uuid::now_v7().simple().to_string();
    let workspace_id = WorkspaceId::new();
    let workspace_slug = WorkspaceSlug::parse(format!("infra-{suffix}"))?;
    let city_slug = CitySlug::parse(format!("city-{suffix}"))?;
    let campaign_id = CampaignId::new();
    let other_campaign_id = CampaignId::new();
    let smart_link_id = SmartLinkId::new();
    seed_acquisition_scope(
        &pool,
        workspace_id,
        &workspace_slug,
        &city_slug,
        campaign_id,
        other_campaign_id,
        smart_link_id,
    )
    .await?;

    let repository = PostgresAcquisitionRepository::new(
        pool.clone(),
        workspace_slug.clone(),
        CountryCode::parse("PL")?,
        &database_config,
        false,
        test_sensitive_response_codec(),
    );

    assert_eq!(
        repository.resolve_workspace(&workspace_slug).await?,
        Some(workspace_id)
    );
    assert_eq!(repository.load_active_smart_links().await?.len(), 1);

    assert_click_batches_are_all_or_nothing(
        &pool,
        &repository,
        workspace_id,
        campaign_id,
        other_campaign_id,
        smart_link_id,
    )
    .await?;

    let first_email = format!("first-{suffix}@example.test");
    let first_command = signup_command(
        workspace_id,
        &first_email,
        &city_slug,
        Some(campaign_id),
        Some(ReferralCode::parse("missing-code")?),
        format!("idem-first-{suffix}"),
        format!("request-first-{suffix}"),
    )?;
    let first_result = repository.persist_fan_signup(&first_command).await?;
    assert!(first_result.created);
    assert_eq!(first_result.status, FanStatus::Active);
    let stored_response = sqlx::query_scalar::<_, String>(
        r#"
        SELECT response_body::text
        FROM idempotency_keys
        WHERE workspace_id = $1 AND scope = 'fan_signup' AND key = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(first_command.idempotency_key().as_str())
    .fetch_one(&pool)
    .await?;
    let session_token = first_result
        .fan_session_token
        .as_ref()
        .ok_or("active signup must issue a session")?;
    assert!(
        !stored_response.contains(session_token.as_str()),
        "fan session token must not be retained as plaintext"
    );
    assert!(stored_response.contains("\"alg\": \"XChaCha20-Poly1305\""));

    assert_first_signup_state(&pool, workspace_id, first_result.fan_id.into_uuid()).await?;

    sqlx::query("UPDATE campaigns SET active = false WHERE workspace_id = $1 AND id = $2")
        .bind(workspace_id.into_uuid())
        .bind(campaign_id.into_uuid())
        .execute(&pool)
        .await?;
    sqlx::query(
        r#"
        UPDATE idempotency_keys
        SET response_body = $3, response_content_type = 'application/json'
        WHERE workspace_id = $1 AND scope = 'fan_signup' AND key = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(first_command.idempotency_key().as_str())
    .bind(serde_json::to_value(&first_result)?)
    .execute(&pool)
    .await?;

    let replay = repository.persist_fan_signup(&first_command).await?;
    assert_eq!(replay, first_result);
    let migrated_response = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT response_content_type, response_body::text
        FROM idempotency_keys
        WHERE workspace_id = $1 AND scope = 'fan_signup' AND key = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(first_command.idempotency_key().as_str())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        migrated_response.0,
        "application/vnd.crowdrelay.encrypted+json"
    );
    assert!(!migrated_response.1.contains(session_token.as_str()));

    let changed_body = signup_command(
        workspace_id,
        &format!("changed-{suffix}@example.test"),
        &city_slug,
        Some(campaign_id),
        None,
        format!("idem-first-{suffix}"),
        format!("request-changed-{suffix}"),
    )?;
    assert_eq!(
        repository.persist_fan_signup(&changed_body).await,
        Err(RepositoryError::Conflict)
    );

    assert_active_fan_signup_is_a_safe_noop(
        &pool,
        &repository,
        workspace_id,
        &first_email,
        first_result.fan_id.into_uuid(),
        &suffix,
    )
    .await?;
    assert_suppressed_fan_is_a_hard_stop(&pool, &repository, workspace_id, &city_slug, &suffix)
        .await?;
    assert_unsubscribed_fan_requires_fresh_inbox_proof(
        &pool,
        &repository,
        workspace_id,
        &city_slug,
        &suffix,
    )
    .await?;
    assert_existing_pending_fan_still_requires_inbox_proof(
        &pool,
        &repository,
        workspace_id,
        &city_slug,
        &suffix,
    )
    .await?;
    assert_concurrent_signup_has_one_creation_and_one_safe_noop(
        &pool,
        &repository,
        workspace_id,
        &city_slug,
        &suffix,
    )
    .await?;
    let double_opt_in_repository = PostgresAcquisitionRepository::new(
        pool.clone(),
        workspace_slug,
        CountryCode::parse("PL")?,
        &database_config,
        true,
        test_sensitive_response_codec(),
    );
    assert_pending_fan_signup_has_a_bounded_safe_resend(
        &pool,
        &double_opt_in_repository,
        workspace_id,
        &city_slug,
        &suffix,
    )
    .await?;

    pool.close().await;
    Ok(())
}

async fn seed_acquisition_scope(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    workspace_slug: &WorkspaceSlug,
    city_slug: &CitySlug,
    campaign_id: CampaignId,
    other_campaign_id: CampaignId,
    smart_link_id: SmartLinkId,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Infra test')")
        .bind(workspace_id.into_uuid())
        .bind(workspace_slug.as_str())
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO cities (slug, name, country_code) VALUES ($1, 'Test city', 'PL')")
        .bind(city_slug.as_str())
        .execute(pool)
        .await?;
    sqlx::query(
        r#"
        INSERT INTO campaigns (id, workspace_id, name)
        VALUES ($1, $3, 'Primary'), ($2, $3, 'Other')
        "#,
    )
    .bind(campaign_id.into_uuid())
    .bind(other_campaign_id.into_uuid())
    .bind(workspace_id.into_uuid())
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO smart_links (
            id,
            workspace_id,
            campaign_id,
            slug,
            destination_url
        )
        VALUES ($1, $2, $3, 'infra-test', 'https://example.test/destination')
        "#,
    )
    .bind(smart_link_id.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(campaign_id.into_uuid())
    .execute(pool)
    .await?;
    Ok(())
}

async fn assert_click_batches_are_all_or_nothing(
    pool: &PgPool,
    repository: &PostgresAcquisitionRepository,
    workspace_id: WorkspaceId,
    campaign_id: CampaignId,
    other_campaign_id: CampaignId,
    smart_link_id: SmartLinkId,
) -> Result<(), Box<dyn std::error::Error>> {
    let valid_link = ResolvedSmartLink::new(
        smart_link_id,
        workspace_id,
        Some(campaign_id),
        SmartLinkSlug::parse("infra-test")?,
        DestinationUrl::parse("https://example.test/destination")?,
        1,
    )?;
    let valid_click = ClickEvent::from_link(
        &valid_link,
        Some(VisitorId::new()),
        Some("example.test".to_owned()),
        OffsetDateTime::now_utc(),
    )?;
    repository
        .persist_click_batch(std::slice::from_ref(&valid_click))
        .await?;

    let inconsistent_link = ResolvedSmartLink::new(
        smart_link_id,
        workspace_id,
        Some(other_campaign_id),
        SmartLinkSlug::parse("infra-test")?,
        DestinationUrl::parse("https://example.test/destination")?,
        1,
    )?;
    let inconsistent_click =
        ClickEvent::from_link(&inconsistent_link, None, None, OffsetDateTime::now_utc())?;
    assert_eq!(
        repository
            .persist_click_batch(&[valid_click, inconsistent_click])
            .await,
        Err(RepositoryError::Conflict)
    );

    let count =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM click_events WHERE workspace_id = $1")
            .bind(workspace_id.into_uuid())
            .fetch_one(pool)
            .await?;
    assert_eq!(count, 1, "an invalid mixed batch must insert no rows");
    Ok(())
}

fn signup_command(
    workspace_id: WorkspaceId,
    email: &str,
    city_slug: &CitySlug,
    campaign_id: Option<CampaignId>,
    claimed_referral_code: Option<ReferralCode>,
    idempotency_key: String,
    request_id: String,
) -> Result<SignupFanCommand, Box<dyn std::error::Error>> {
    let signup = FanSignup::new(FanSignupInput {
        workspace_id,
        email: NormalizedEmail::parse(email)?,
        display_name: Some("Test fan".to_owned()),
        city_slug: city_slug.clone(),
        locale: Some("pl-PL".to_owned()),
        campaign_id,
        visitor_id: Some(VisitorId::new()),
        claimed_referral_code,
        consent: MarketingConsent::new(true, "privacy-v1", "integration-test")?,
    })?;
    Ok(SignupFanCommand::new(
        IdempotencyKey::parse(idempotency_key)?,
        RequestId::parse(request_id)?,
        signup,
    ))
}

async fn assert_pending_fan_signup_has_a_bounded_safe_resend(
    pool: &PgPool,
    repository: &PostgresAcquisitionRepository,
    workspace_id: WorkspaceId,
    city_slug: &CitySlug,
    suffix: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let email = format!("pending-safe-{suffix}@example.test");
    let first = signup_command(
        workspace_id,
        &email,
        city_slug,
        None,
        None,
        format!("idem-pending-first-{suffix}"),
        format!("request-pending-first-{suffix}"),
    )?;
    let first_result = repository.persist_fan_signup(&first).await?;
    assert!(first_result.created);
    assert_eq!(first_result.status, FanStatus::Pending);
    assert!(first_result.confirmation_required);

    let injected_city = CitySlug::parse(format!("pending-injected-{suffix}"))?;
    sqlx::query("INSERT INTO cities (slug, name, country_code) VALUES ($1, 'Injected city', 'PL')")
        .bind(injected_city.as_str())
        .execute(pool)
        .await?;
    let retry = SignupFanCommand::new(
        IdempotencyKey::parse(format!("idem-pending-retry-{suffix}"))?,
        RequestId::parse(format!("request-pending-retry-{suffix}"))?,
        FanSignup::new(FanSignupInput {
            workspace_id,
            email: NormalizedEmail::parse(&email)?,
            display_name: Some("Injected profile".to_owned()),
            city_slug: injected_city,
            locale: Some("en-US".to_owned()),
            campaign_id: None,
            visitor_id: Some(VisitorId::new()),
            claimed_referral_code: None,
            consent: MarketingConsent::new(true, "privacy-injected-v2", "untrusted-pending-retry")?,
        })?,
    );

    let before = pending_fan_snapshot(pool, workspace_id, first_result.fan_id.into_uuid()).await?;
    let retry_result = repository.persist_fan_signup(&retry).await?;
    assert_eq!(retry_result.fan_id, first_result.fan_id);
    assert_eq!(retry_result.status, FanStatus::Pending);
    assert!(!retry_result.created);
    assert!(retry_result.confirmation_required);
    assert_eq!(retry_result.referral_code, None);
    assert_eq!(retry_result.fan_session_token, None);
    assert_eq!(
        pending_fan_snapshot(pool, workspace_id, first_result.fan_id.into_uuid()).await?,
        before,
        "a repeated pending signup inside the resend cooldown must not rotate tokens, send mail, or mutate attribution"
    );
    Ok(())
}

async fn pending_fan_snapshot(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    fan_id: Uuid,
) -> Result<PendingFanSnapshot, sqlx::Error> {
    let (display_name, locale, consent_count, acquisition_count, interest_count, outbox_count) =
        sqlx::query_as::<_, (Option<String>, Option<String>, i64, i64, i64, i64)>(
            r#"
            SELECT
                fan.display_name,
                fan.locale,
                (SELECT count(*) FROM fan_consents
                    WHERE workspace_id = $1 AND fan_id = $2),
                (SELECT count(*) FROM fan_acquisition_events
                    WHERE workspace_id = $1 AND fan_id = $2),
                (SELECT count(*) FROM fan_city_interests
                    WHERE workspace_id = $1 AND fan_id = $2),
                (SELECT count(*) FROM outbox_events
                    WHERE workspace_id = $1
                        AND event_type = 'fan.confirmation_requested'
                        AND payload ->> 'fan_id' = $2::text)
            FROM fans AS fan
            WHERE fan.workspace_id = $1 AND fan.id = $2
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id)
        .fetch_one(pool)
        .await?;
    let tokens = sqlx::query_as::<_, (Uuid, Vec<u8>, OffsetDateTime)>(
        r#"
        SELECT id, token_hash, expires_at
        FROM fan_action_tokens
        WHERE workspace_id = $1
            AND fan_id = $2
            AND purpose = 'confirm'
            AND consumed_at IS NULL
        ORDER BY id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .fetch_all(pool)
    .await?;
    Ok(PendingFanSnapshot {
        display_name,
        locale,
        consent_count,
        acquisition_count,
        interest_count,
        outbox_count,
        tokens,
    })
}

#[derive(Debug, PartialEq)]
struct PendingFanSnapshot {
    display_name: Option<String>,
    locale: Option<String>,
    consent_count: i64,
    acquisition_count: i64,
    interest_count: i64,
    outbox_count: i64,
    tokens: Vec<(Uuid, Vec<u8>, OffsetDateTime)>,
}

async fn assert_first_signup_state(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    fan_id: Uuid,
) -> Result<(), Box<dyn std::error::Error>> {
    let (fan_count, consent_count, acquisition_count, outbox_count, city_count) =
        sqlx::query_as::<_, (i64, i64, i64, i64, i64)>(
            r#"
            SELECT
                (SELECT count(*) FROM fans
                    WHERE workspace_id = $1 AND id = $2),
                (SELECT count(*) FROM fan_consents
                    WHERE workspace_id = $1 AND fan_id = $2),
                (SELECT count(*) FROM fan_acquisition_events
                    WHERE workspace_id = $1 AND fan_id = $2
                        AND referral_code_id IS NULL),
                (SELECT count(*) FROM outbox_events
                    WHERE workspace_id = $1
                        AND event_type = 'fan.created'
                        AND payload ->> 'fan_id' = $2::text),
                (SELECT coalesce(sum(confirmed_fan_count), 0)::bigint
                    FROM city_aggregates WHERE workspace_id = $1)
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id)
        .fetch_one(pool)
        .await?;
    assert_eq!(
        (fan_count, consent_count, acquisition_count, outbox_count),
        (1, 1, 1, 1)
    );
    assert_eq!(city_count, 1);

    let (has_visitor, has_consent, policy_version) =
        sqlx::query_as::<_, (bool, bool, Option<String>)>(
            r#"
            SELECT
                payload ? 'visitor_id',
                payload ? 'consent',
                payload ->> 'policy_version'
            FROM outbox_events
            WHERE workspace_id = $1
                AND event_type = 'fan.created'
                AND payload ->> 'fan_id' = $2::text
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id)
        .fetch_one(pool)
        .await?;
    assert!(!has_visitor);
    assert!(!has_consent);
    assert_eq!(policy_version.as_deref(), Some("privacy-v1"));
    Ok(())
}

async fn assert_active_fan_signup_is_a_safe_noop(
    pool: &PgPool,
    repository: &PostgresAcquisitionRepository,
    workspace_id: WorkspaceId,
    email: &str,
    fan_id: Uuid,
    suffix: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let other_city_slug = CitySlug::parse(format!("active-noop-{suffix}"))?;
    sqlx::query("INSERT INTO cities (slug, name, country_code) VALUES ($1, 'No-op city', 'PL')")
        .bind(other_city_slug.as_str())
        .execute(pool)
        .await?;

    let referrer_id = Uuid::now_v7();
    let referral_code = ReferralCode::parse(format!("noop-ref-{suffix}"))?;
    sqlx::query(
        r#"
        INSERT INTO fans (
            id,
            workspace_id,
            normalized_email,
            display_name,
            locale,
            status
        )
        VALUES ($1, $2, $3, 'Legitimate referrer', 'pl-PL', 'active')
        "#,
    )
    .bind(referrer_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("noop-referrer-{suffix}@example.test"))
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO referral_codes (workspace_id, fan_id, code)
        VALUES ($1, $2, $3)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(referrer_id)
    .bind(referral_code.as_str())
    .execute(pool)
    .await?;

    let before = active_fan_snapshot(pool, workspace_id, fan_id).await?;
    let first = active_retry_command(
        workspace_id,
        email,
        &other_city_slug,
        &referral_code,
        "Injected profile A",
        &format!("active-noop-a-{suffix}"),
    )?;
    let second = active_retry_command(
        workspace_id,
        email,
        &other_city_slug,
        &referral_code,
        "Injected profile B",
        &format!("active-noop-b-{suffix}"),
    )?;

    let (first_result, second_result) = tokio::join!(
        repository.persist_fan_signup(&first),
        repository.persist_fan_signup(&second)
    );
    let first_result = first_result?;
    let second_result = second_result?;
    for result in [&first_result, &second_result] {
        assert_eq!(result.fan_id.into_uuid(), fan_id);
        assert_eq!(result.status, FanStatus::Active);
        assert!(!result.created);
        assert!(result.confirmation_required);
        assert_eq!(result.referral_code, None);
        assert_eq!(result.fan_session_token, None);
    }

    assert_eq!(
        repository.persist_fan_signup(&first).await?,
        first_result,
        "an exact replay must return the stored no-op result"
    );
    assert_eq!(
        repository.persist_fan_signup(&second).await?,
        second_result,
        "each idempotency key must replay its own stored no-op result"
    );
    let after = active_fan_snapshot(pool, workspace_id, fan_id).await?;
    assert_eq!(after.profile, before.profile);
    assert_eq!(after.fan_owned_counts, before.fan_owned_counts);
    assert_eq!(after.sessions, before.sessions);
    assert_eq!(
        (
            after.workspace_counts.1,
            after.workspace_counts.2,
            after.workspace_counts.3,
            after.workspace_counts.4,
        ),
        (
            before.workspace_counts.1,
            before.workspace_counts.2,
            before.workspace_counts.3,
            before.workspace_counts.4,
        )
    );
    assert_eq!(after.workspace_counts.0, before.workspace_counts.0 + 1);
    assert_eq!(after.action_tokens.len(), before.action_tokens.len() + 1);
    let recovery_tokens = after
        .action_tokens
        .iter()
        .filter(|token| token.1 == "session" && token.4.is_none())
        .count();
    assert_eq!(recovery_tokens, 1);
    let recovery_events = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM outbox_events \
         WHERE workspace_id = $1 AND event_type = 'fan.session_requested' \
           AND payload ->> 'fan_id' = $2::text",
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(recovery_events, 1);
    Ok(())
}

fn active_retry_command(
    workspace_id: WorkspaceId,
    email: &str,
    city_slug: &CitySlug,
    referral_code: &ReferralCode,
    display_name: &str,
    key_tag: &str,
) -> Result<SignupFanCommand, Box<dyn std::error::Error>> {
    let signup = FanSignup::new(FanSignupInput {
        workspace_id,
        email: NormalizedEmail::parse(email)?,
        display_name: Some(display_name.to_owned()),
        city_slug: city_slug.clone(),
        locale: Some("en-US".to_owned()),
        campaign_id: None,
        visitor_id: Some(VisitorId::new()),
        claimed_referral_code: Some(referral_code.clone()),
        consent: MarketingConsent::new(true, "privacy-attacker-v2", "untrusted-retry")?,
    })?;
    Ok(SignupFanCommand::new(
        IdempotencyKey::parse(format!("idem-{key_tag}"))?,
        RequestId::parse(format!("request-{key_tag}"))?,
        signup,
    ))
}

#[derive(Debug, PartialEq)]
struct ActiveFanSnapshot {
    profile: (Option<String>, Option<String>, String, OffsetDateTime),
    fan_owned_counts: (i64, i64, i64, i64, i64),
    workspace_counts: (i64, i64, i64, i64, i64),
    sessions: Vec<SessionSnapshot>,
    action_tokens: Vec<ActionTokenSnapshot>,
}

type SessionSnapshot = (Uuid, Vec<u8>, OffsetDateTime, Option<OffsetDateTime>);
type ActionTokenSnapshot = (
    Uuid,
    String,
    Vec<u8>,
    OffsetDateTime,
    Option<OffsetDateTime>,
);

async fn active_fan_snapshot(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    fan_id: Uuid,
) -> Result<ActiveFanSnapshot, Box<dyn std::error::Error>> {
    let profile = sqlx::query_as::<_, (Option<String>, Option<String>, String, OffsetDateTime)>(
        r#"
        SELECT display_name, locale, status, updated_at
        FROM fans
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .fetch_one(pool)
    .await?;
    let fan_owned_counts = sqlx::query_as::<_, (i64, i64, i64, i64, i64)>(
        r#"
        SELECT
            (SELECT count(*) FROM fan_consents
                WHERE workspace_id = $1 AND fan_id = $2),
            (SELECT count(*) FROM fan_acquisition_events
                WHERE workspace_id = $1 AND fan_id = $2),
            (SELECT count(*) FROM fan_city_interests
                WHERE workspace_id = $1 AND fan_id = $2),
            (SELECT count(*) FROM referral_codes
                WHERE workspace_id = $1 AND fan_id = $2),
            (SELECT count(*) FROM referral_attributions
                WHERE workspace_id = $1 AND referred_fan_id = $2)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .fetch_one(pool)
    .await?;
    let workspace_counts = sqlx::query_as::<_, (i64, i64, i64, i64, i64)>(
        r#"
        SELECT
            (SELECT count(*) FROM outbox_events WHERE workspace_id = $1),
            (SELECT count(*) FROM reward_grants WHERE workspace_id = $1),
            (SELECT count(*) FROM merch_coupons WHERE workspace_id = $1),
            (SELECT count(*) FROM city_aggregates WHERE workspace_id = $1),
            (SELECT coalesce(sum(confirmed_fan_count), 0)::bigint
                FROM city_aggregates WHERE workspace_id = $1)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(pool)
    .await?;
    let sessions = sqlx::query_as::<_, SessionSnapshot>(
        r#"
        SELECT id, session_token_hash, expires_at, revoked_at
        FROM fan_sessions
        WHERE workspace_id = $1 AND fan_id = $2
        ORDER BY id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .fetch_all(pool)
    .await?;
    let action_tokens = sqlx::query_as::<_, ActionTokenSnapshot>(
        r#"
        SELECT id, purpose, token_hash, expires_at, consumed_at
        FROM fan_action_tokens
        WHERE workspace_id = $1 AND fan_id = $2
        ORDER BY id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .fetch_all(pool)
    .await?;

    Ok(ActiveFanSnapshot {
        profile,
        fan_owned_counts,
        workspace_counts,
        sessions,
        action_tokens,
    })
}

async fn assert_suppressed_fan_is_a_hard_stop(
    pool: &PgPool,
    repository: &PostgresAcquisitionRepository,
    workspace_id: WorkspaceId,
    city_slug: &CitySlug,
    suffix: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let fan_id = Uuid::now_v7();
    let email = format!("suppressed-{suffix}@example.test");
    sqlx::query(
        r#"
        INSERT INTO fans (id, workspace_id, normalized_email, status)
        VALUES ($1, $2, $3, 'suppressed')
        "#,
    )
    .bind(fan_id)
    .bind(workspace_id.into_uuid())
    .bind(&email)
    .execute(pool)
    .await?;

    let command = signup_command(
        workspace_id,
        &email,
        city_slug,
        None,
        None,
        format!("idem-suppressed-{suffix}"),
        format!("request-suppressed-{suffix}"),
    )?;
    assert_eq!(
        repository.persist_fan_signup(&command).await,
        Err(RepositoryError::Conflict)
    );
    let writes = sqlx::query_as::<_, (i64, i64, i64)>(
        r#"
        SELECT
            (SELECT count(*) FROM fan_consents
                WHERE workspace_id = $1 AND fan_id = $2),
            (SELECT count(*) FROM fan_acquisition_events
                WHERE workspace_id = $1 AND fan_id = $2),
            (SELECT count(*) FROM referral_codes
                WHERE workspace_id = $1 AND fan_id = $2)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(writes, (0, 0, 0));
    Ok(())
}

async fn assert_unsubscribed_fan_requires_fresh_inbox_proof(
    pool: &PgPool,
    repository: &PostgresAcquisitionRepository,
    workspace_id: WorkspaceId,
    city_slug: &CitySlug,
    suffix: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let fan_id = Uuid::now_v7();
    let email = format!("unsubscribed-{suffix}@example.test");
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status) \
         VALUES ($1, $2, $3, 'unsubscribed')",
    )
    .bind(fan_id)
    .bind(workspace_id.into_uuid())
    .bind(&email)
    .execute(pool)
    .await?;
    let city_before = city_count_for_slug(pool, workspace_id, city_slug).await?;
    let result = repository
        .persist_fan_signup(&signup_command(
            workspace_id,
            &email,
            city_slug,
            None,
            None,
            format!("idem-unsubscribed-{suffix}"),
            format!("request-unsubscribed-{suffix}"),
        )?)
        .await?;

    assert_eq!(result.fan_id.into_uuid(), fan_id);
    assert_eq!(result.status, FanStatus::Pending);
    assert!(result.confirmation_required);
    assert!(result.fan_session_token.is_none());
    assert!(result.referral_code.is_none());
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT status FROM fans WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id)
        .fetch_one(pool)
        .await?,
        "pending"
    );
    assert_eq!(
        city_count_for_slug(pool, workspace_id, city_slug).await?,
        city_before
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*)::bigint FROM outbox_events \
             WHERE workspace_id = $1 AND event_type = 'fan.confirmation_requested' \
               AND payload ->> 'fan_id' = $2::text",
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id)
        .fetch_one(pool)
        .await?,
        1
    );
    Ok(())
}

async fn assert_existing_pending_fan_still_requires_inbox_proof(
    pool: &PgPool,
    repository: &PostgresAcquisitionRepository,
    workspace_id: WorkspaceId,
    city_slug: &CitySlug,
    suffix: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let existing_city_slug = CitySlug::parse(format!("existing-interest-{suffix}"))?;
    let requested_city_slug = CitySlug::parse(format!("requested-interest-{suffix}"))?;
    sqlx::query(
        r#"
        INSERT INTO cities (slug, name, country_code)
        VALUES
            ($1, 'Existing interest city', 'PL'),
            ($2, 'Requested interest city', 'PL')
        "#,
    )
    .bind(existing_city_slug.as_str())
    .bind(requested_city_slug.as_str())
    .execute(pool)
    .await?;

    let fan_id = Uuid::now_v7();
    let email = format!("pending-{suffix}@example.test");
    sqlx::query(
        r#"
        INSERT INTO fans (id, workspace_id, normalized_email, status)
        VALUES ($1, $2, $3, 'pending')
        "#,
    )
    .bind(fan_id)
    .bind(workspace_id.into_uuid())
    .bind(&email)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO fan_city_interests (workspace_id, fan_id, city_id)
        SELECT $1, $2, id
        FROM cities
        WHERE country_code = 'PL'
            AND slug IN ($3, $4)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .bind(city_slug.as_str())
    .bind(existing_city_slug.as_str())
    .execute(pool)
    .await?;

    let primary_before = city_count_for_slug(pool, workspace_id, city_slug).await?;
    let existing_before = city_count_for_slug(pool, workspace_id, &existing_city_slug).await?;
    let requested_before = city_count_for_slug(pool, workspace_id, &requested_city_slug).await?;

    let first = signup_command(
        workspace_id,
        &email,
        &requested_city_slug,
        None,
        None,
        format!("idem-reactivate-a-{suffix}"),
        format!("request-reactivate-a-{suffix}"),
    )?;
    let second = signup_command(
        workspace_id,
        &email,
        &requested_city_slug,
        None,
        None,
        format!("idem-reactivate-b-{suffix}"),
        format!("request-reactivate-b-{suffix}"),
    )?;
    let (first_result, second_result) = tokio::join!(
        repository.persist_fan_signup(&first),
        repository.persist_fan_signup(&second)
    );
    let first_result = first_result?;
    let second_result = second_result?;
    for result in [&first_result, &second_result] {
        assert_eq!(result.fan_id.into_uuid(), fan_id);
        assert_eq!(result.status, FanStatus::Pending);
        assert!(!result.created);
        assert!(result.confirmation_required);
        assert!(result.fan_session_token.is_none());
        assert!(result.referral_code.is_none());
    }

    assert_eq!(
        city_count_for_slug(pool, workspace_id, city_slug).await?,
        primary_before
    );
    assert_eq!(
        city_count_for_slug(pool, workspace_id, &existing_city_slug).await?,
        existing_before
    );
    assert_eq!(
        city_count_for_slug(pool, workspace_id, &requested_city_slug).await?,
        requested_before
    );
    let interest_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM fan_city_interests WHERE workspace_id = $1 AND fan_id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(interest_count, 2);
    let confirmation_event_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM outbox_events \
         WHERE workspace_id = $1 AND event_type = 'fan.confirmation_requested' \
           AND payload ->> 'fan_id' = $2::text",
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(confirmation_event_count, 1);

    let counts_after = (
        city_count_for_slug(pool, workspace_id, city_slug).await?,
        city_count_for_slug(pool, workspace_id, &existing_city_slug).await?,
        city_count_for_slug(pool, workspace_id, &requested_city_slug).await?,
    );
    assert_eq!(repository.persist_fan_signup(&first).await?, first_result);
    assert_eq!(repository.persist_fan_signup(&second).await?, second_result);
    assert_eq!(
        (
            city_count_for_slug(pool, workspace_id, city_slug).await?,
            city_count_for_slug(pool, workspace_id, &existing_city_slug).await?,
            city_count_for_slug(pool, workspace_id, &requested_city_slug).await?,
        ),
        counts_after,
        "replaying either concurrent key must not increment aggregates again"
    );
    Ok(())
}

async fn assert_concurrent_signup_has_one_creation_and_one_safe_noop(
    pool: &PgPool,
    repository: &PostgresAcquisitionRepository,
    workspace_id: WorkspaceId,
    city_slug: &CitySlug,
    suffix: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let email = format!("concurrent-{suffix}@example.test");
    let first = signup_command(
        workspace_id,
        &email,
        city_slug,
        None,
        None,
        format!("idem-concurrent-a-{suffix}"),
        format!("request-concurrent-a-{suffix}"),
    )?;
    let second = signup_command(
        workspace_id,
        &email,
        city_slug,
        None,
        None,
        format!("idem-concurrent-b-{suffix}"),
        format!("request-concurrent-b-{suffix}"),
    )?;
    let city_count_before = city_count_for_slug(pool, workspace_id, city_slug).await?;
    let (first_result, second_result) = tokio::join!(
        repository.persist_fan_signup(&first),
        repository.persist_fan_signup(&second)
    );
    let first_result = first_result?;
    let second_result = second_result?;
    assert_eq!(first_result.fan_id, second_result.fan_id);
    assert_ne!(first_result.created, second_result.created);
    let (created, no_op) = if first_result.created {
        (&first_result, &second_result)
    } else {
        (&second_result, &first_result)
    };
    assert_eq!(created.status, FanStatus::Active);
    assert!(created.referral_code.is_some());
    assert!(created.fan_session_token.is_some());
    assert_eq!(no_op.status, FanStatus::Active);
    assert_eq!(no_op.referral_code, None);
    assert_eq!(no_op.fan_session_token, None);
    assert!(no_op.confirmation_required);

    let writes = sqlx::query_as::<_, (i64, i64, i64, i64, i64, i64, i64, i64)>(
        r#"
        SELECT
            (SELECT count(*) FROM fans
                WHERE workspace_id = $1 AND normalized_email = $2),
            (SELECT count(*) FROM referral_codes
                WHERE workspace_id = $1 AND fan_id = $3 AND active),
            (SELECT count(*) FROM outbox_events
                WHERE workspace_id = $1
                    AND event_type = 'fan.created'
                    AND payload ->> 'fan_id' = $3::text),
            (SELECT count(*) FROM fan_consents
                WHERE workspace_id = $1 AND fan_id = $3),
            (SELECT count(*) FROM fan_acquisition_events
                WHERE workspace_id = $1 AND fan_id = $3),
            (SELECT count(*) FROM fan_city_interests
                WHERE workspace_id = $1 AND fan_id = $3),
            (SELECT count(*) FROM fan_sessions
                WHERE workspace_id = $1 AND fan_id = $3),
            (SELECT count(*) FROM fan_action_tokens
                WHERE workspace_id = $1
                    AND fan_id = $3
                    AND purpose = 'unsubscribe'
                    AND consumed_at IS NULL)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&email)
    .bind(first_result.fan_id.into_uuid())
    .fetch_one(pool)
    .await?;
    assert_eq!(writes, (1, 1, 1, 1, 1, 1, 1, 1));
    assert_eq!(
        city_count_for_slug(pool, workspace_id, city_slug).await?,
        city_count_before + 1,
        "the newly active fan must increment only its new city once"
    );
    Ok(())
}

async fn city_count_for_slug(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    city_slug: &CitySlug,
) -> Result<i64, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, i64>(
        r#"
        SELECT coalesce((
            SELECT confirmed_fan_count
            FROM city_aggregates
            INNER JOIN cities ON cities.id = city_aggregates.city_id
            WHERE city_aggregates.workspace_id = $1
                AND cities.slug = $2
        ), 0)::bigint
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(city_slug.as_str())
    .fetch_one(pool)
    .await?)
}

fn test_sensitive_response_codec() -> SensitiveResponseCodec {
    SensitiveResponseCodec::new(SensitiveResponseKey::derive_from_secret(
        b"acquisition-integration-response-secret",
    ))
}
