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

include!("acquisition_postgres/helpers.rs");
