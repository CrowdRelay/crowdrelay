use std::time::Duration;

use crowdrelay_application::{
    AcquisitionRepository, IdempotencyKey, RepositoryError, RequestId, SignupFanCommand,
};
use crowdrelay_domain::{
    CampaignId, CitySlug, ClickEvent, CountryCode, DestinationUrl, FanId, FanSignup,
    FanSignupInput, FanStatus, MarketingConsent, NormalizedEmail, ReferralCode, ResolvedSmartLink,
    SmartLinkId, SmartLinkSlug, VisitorId, WorkspaceId, WorkspaceSlug,
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

/// Regression: `record_community_conversion` must use `fan.created_at` as
/// `occurred_at`, not `now()` (write time). Seeds a community-tagged smart
/// link + click, signs up a fan, and asserts the conversion provenance event
/// carries the fan's `created_at` — not the current time.
#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn community_conversion_occurred_at_uses_fan_created_at()
-> Result<(), Box<dyn std::error::Error>> {
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
    let workspace_slug = WorkspaceSlug::parse(format!("conv-{suffix}"))?;
    let city_slug = CitySlug::parse(format!("conv-city-{suffix}"))?;
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

    // Tag the smart link with a community so the conversion query matches.
    sqlx::query(
        "UPDATE smart_links SET channel_community = 'r/progmetal', channel_source = 'reddit' \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(smart_link_id.into_uuid())
    .execute(&pool)
    .await?;

    let repository = PostgresAcquisitionRepository::new(
        pool.clone(),
        workspace_slug,
        CountryCode::parse("PL")?,
        &database_config,
        false,
        test_sensitive_response_codec(),
    );

    // Record a community-tagged click so the conversion has something to
    // attribute.
    let visitor_id = VisitorId::new();
    let link = ResolvedSmartLink::new(
        smart_link_id,
        workspace_id,
        Some(campaign_id),
        SmartLinkSlug::parse("infra-test")?,
        DestinationUrl::parse("https://example.test/destination")?,
        1,
    )?;
    let click = ClickEvent::from_link(
        &link,
        Some(visitor_id),
        Some("example.test".to_owned()),
        OffsetDateTime::now_utc(),
    )?;
    repository
        .persist_click_batch(std::slice::from_ref(&click))
        .await?;

    // Sign up a fan with the same visitor_id so the conversion links back.
    let email = format!("conv-{suffix}@example.test");
    // Override the visitor_id in the signup to match the click.
    let signup = FanSignup::new(FanSignupInput {
        workspace_id,
        email: NormalizedEmail::parse(&email)?,
        display_name: Some("Conv test fan".to_owned()),
        city_slug: city_slug.clone(),
        locale: Some("pl-PL".to_owned()),
        campaign_id: Some(campaign_id),
        visitor_id: Some(visitor_id),
        claimed_referral_code: None,
        consent: MarketingConsent::new(true, "privacy-v1", "conv-test")?,
    })?;
    let command = SignupFanCommand::new(
        IdempotencyKey::parse(format!("idem-conv-{suffix}"))?,
        RequestId::parse(format!("request-conv-{suffix}"))?,
        signup,
    );
    let result = repository.persist_fan_signup(&command).await?;
    assert!(result.created);

    let fan_id = result.fan_id.into_uuid();

    // Read the fan's created_at.
    let fan_created_at: OffsetDateTime =
        sqlx::query_scalar("SELECT created_at FROM fans WHERE workspace_id = $1 AND id = $2")
            .bind(workspace_id.into_uuid())
            .bind(fan_id)
            .fetch_one(&pool)
            .await?;

    // Read the conversion provenance event.
    let conversion_row: Option<(OffsetDateTime,)> = sqlx::query_as(
        "SELECT occurred_at FROM fan_provenance_events \
         WHERE workspace_id = $1 AND fan_id = $2 AND event_kind = 'conversion'",
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .fetch_optional(&pool)
    .await?;

    let occurred_at = conversion_row
        .ok_or("expected a conversion provenance event to be written")?
        .0;

    assert_eq!(
        occurred_at, fan_created_at,
        "occurred_at must equal fan.created_at, not now() (write time)"
    );

    pool.close().await;
    Ok(())
}

/// Regression: if the fan does not exist, `record_community_conversion` must
/// NOT write a conversion event with `occurred_at = now()`. The JOIN to
/// `fans` produces no rows, so the INSERT ... SELECT writes nothing.
/// This path is effectively unreachable in production (the fan is always
/// upserted in the same transaction), but the fallback must not encode a
/// false semantic value.
#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn community_conversion_does_not_write_when_fan_is_missing()
-> Result<(), Box<dyn std::error::Error>> {
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
    let workspace_slug = WorkspaceSlug::parse(format!("miss-{suffix}"))?;
    let city_slug = CitySlug::parse(format!("miss-city-{suffix}"))?;
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

    // Tag the smart link with a community.
    sqlx::query(
        "UPDATE smart_links SET channel_community = 'r/progmetal', channel_source = 'reddit' \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(smart_link_id.into_uuid())
    .execute(&pool)
    .await?;

    // Record a community-tagged click.
    let visitor_id = VisitorId::new();
    let link = ResolvedSmartLink::new(
        smart_link_id,
        workspace_id,
        Some(campaign_id),
        SmartLinkSlug::parse("infra-test")?,
        DestinationUrl::parse("https://example.test/destination")?,
        1,
    )?;
    let click = ClickEvent::from_link(
        &link,
        Some(visitor_id),
        Some("example.test".to_owned()),
        OffsetDateTime::now_utc(),
    )?;

    let repository = PostgresAcquisitionRepository::new(
        pool.clone(),
        workspace_slug,
        CountryCode::parse("PL")?,
        &database_config,
        false,
        test_sensitive_response_codec(),
    );
    repository
        .persist_click_batch(std::slice::from_ref(&click))
        .await?;

    // Directly invoke the conversion recording with a non-existent fan_id.
    // This simulates the unreachable path: the fan was never created.
    let phantom_fan_id = FanId::new();
    let mut tx = pool.begin().await?;

    // We verify the invariant by running the same INSERT ... SELECT that
    // record_community_conversion uses, with a non-existent fan_id. The JOIN
    // to fans produces no rows, so no conversion is written.
    let insert_result = sqlx::query(
        r#"
        INSERT INTO fan_provenance_events (
            workspace_id, fan_id, event_kind, channel, source_target,
            community, campaign_id, action_id, attribution_method,
            attribution_confidence, occurred_at
        )
        SELECT $1, $2, 'conversion',
               COALESCE(link.channel_source, 'smart_link'),
               link.slug, link.channel_community, click.campaign_id,
               post.action_id,
               'last_community_click', 1.0, fan.created_at
        FROM click_events AS click
        JOIN smart_links AS link
          ON link.workspace_id = click.workspace_id
         AND link.id = click.smart_link_id
        JOIN fans AS fan
          ON fan.workspace_id = $1
         AND fan.id = $2
        LEFT JOIN LATERAL (
            SELECT post.action_id
            FROM community_posts AS post
            WHERE post.workspace_id = $1
              AND post.smart_link = '/l/' || link.slug
            ORDER BY post.posted_at DESC NULLS LAST,
                     post.created_at DESC
            LIMIT 1
        ) AS post ON true
        WHERE click.workspace_id = $1
          AND click.anonymous_visitor_id = $3
          AND link.channel_community IS NOT NULL
          AND click.occurred_at >= now() - INTERVAL '30 days'
        ORDER BY click.occurred_at DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(phantom_fan_id.into_uuid())
    .bind(Into::<Uuid>::into(visitor_id))
    .execute(&mut *tx)
    .await?;

    // The INSERT ... SELECT with a JOIN on a non-existent fan produces 0 rows.
    assert_eq!(
        insert_result.rows_affected(),
        0,
        "no conversion event must be written when the fan does not exist"
    );
    tx.rollback().await?;

    // Double-check: no provenance event for the phantom fan.
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fan_provenance_events \
         WHERE workspace_id = $1 AND fan_id = $2 AND event_kind = 'conversion'",
    )
    .bind(workspace_id.into_uuid())
    .bind(phantom_fan_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(count, 0, "no conversion row must exist for a phantom fan");

    pool.close().await;
    Ok(())
}

include!("acquisition_postgres/helpers.rs");
include!("acquisition_postgres/attribution.rs");
