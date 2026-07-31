use std::time::Duration;

use crowdrelay_application::{
    AcquisitionRepository, ConfirmFanCommand, FanLifecycleRepository, IdempotencyKey,
    RepositoryError, RequestId, SignupFanCommand,
};
use crowdrelay_domain::{
    CitySlug, CountryCode, FanActionToken, FanSignup, FanSignupInput, FanStatus, MarketingConsent,
    NormalizedEmail, WorkspaceId, WorkspaceSlug,
};
use crowdrelay_infra::{
    acquisition::PostgresAcquisitionRepository,
    config::DatabaseConfig,
    fan_lifecycle::PostgresFanLifecycleRepository,
    sensitive_response::{SensitiveResponseCodec, SensitiveResponseKey},
};
use serde_json::Value;
use sqlx::{PgPool, postgres::PgPoolOptions};

#[tokio::test]
#[ignore = "requires CROWDRELAY_FAN_LIFECYCLE_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn pending_fan_confirms_once_and_unsubscribes_idempotently()
-> Result<(), Box<dyn std::error::Error>> {
    let database_url =
        std::env::var("CROWDRELAY_FAN_LIFECYCLE_TEST_DATABASE_URL").map_err(|e| {
            format!("CROWDRELAY_FAN_LIFECYCLE_TEST_DATABASE_URL must be configured: {e}")
        })?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    let workspace_slug = WorkspaceSlug::parse(format!("lifecycle-{suffix}"))?;
    seed_workspace(&pool, workspace_id, &workspace_slug).await?;

    let database = test_database_config(database_url);
    let acquisition = PostgresAcquisitionRepository::new(
        pool.clone(),
        workspace_slug.clone(),
        CountryCode::parse("PL")?,
        &database,
        true,
        test_sensitive_response_codec(),
    );
    let lifecycle = PostgresFanLifecycleRepository::new(
        pool.clone(),
        workspace_slug,
        &database,
        test_sensitive_response_codec(),
    );

    let signup = acquisition
        .persist_fan_signup(&signup_command(workspace_id, &suffix)?)
        .await?;
    assert_eq!(signup.status, FanStatus::Pending);
    assert!(signup.confirmation_required);
    assert!(signup.fan_session_token.is_none());
    assert_eq!(city_count(&pool, workspace_id).await?, 0);

    let confirmation_token = outbox_token(
        &pool,
        workspace_id,
        "fan.confirmation_requested",
        "confirmation_token",
    )
    .await?;
    let confirmation_token = FanActionToken::parse(confirmation_token)?;
    let confirmation = confirmation_command(
        workspace_id,
        confirmation_token.clone(),
        "primary-confirmation",
    )?;
    let confirmed = lifecycle.confirm(&confirmation).await?;
    assert_eq!(confirmed.status, FanStatus::Active);
    assert_eq!(city_count(&pool, workspace_id).await?, 1);
    let replayed = lifecycle
        .confirm(&ConfirmFanCommand {
            request_id: RequestId::parse("request-primary-confirmation-retry")?,
            ..confirmation.clone()
        })
        .await?;
    assert_eq!(confirmed, replayed);
    assert_eq!(city_count(&pool, workspace_id).await?, 1);
    let (idempotency_body, idempotency_content_type) = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT response_body::text, response_content_type
        FROM idempotency_keys
        WHERE workspace_id = $1 AND scope = 'fan.confirm' AND key = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(confirmation.idempotency_key.as_str())
    .fetch_one(&pool)
    .await?;
    assert!(!idempotency_body.contains(confirmed.fan_session_token.as_str()));
    assert!(idempotency_body.contains("\"alg\": \"XChaCha20-Poly1305\""));
    assert_eq!(
        idempotency_content_type,
        "application/vnd.crowdrelay.encrypted+json"
    );
    let confirmed_event_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM outbox_events \
         WHERE workspace_id = $1 AND event_type = 'fan.confirmed'",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(confirmed_event_count, 1);
    assert_eq!(
        lifecycle
            .confirm(&ConfirmFanCommand {
                token: FanActionToken::parse("f".repeat(64))?,
                request_id: RequestId::parse("request-primary-confirmation-conflict")?,
                ..confirmation
            })
            .await,
        Err(RepositoryError::Conflict)
    );

    let recovery_token = FanActionToken::parse("2".repeat(64))?;
    insert_action_token(
        &pool,
        workspace_id,
        signup.fan_id.into_uuid(),
        "session",
        &recovery_token,
    )
    .await?;
    let recovery = confirmation_command(workspace_id, recovery_token, "primary-session-recovery")?;
    let recovered = lifecycle.confirm(&recovery).await?;
    assert_eq!(recovered.status, FanStatus::Active);
    assert_ne!(recovered.fan_session_token, confirmed.fan_session_token);
    assert_eq!(recovered.referral_code, confirmed.referral_code);
    assert_eq!(city_count(&pool, workspace_id).await?, 1);
    assert_eq!(
        lifecycle
            .confirm(&ConfirmFanCommand {
                request_id: RequestId::parse("request-primary-session-recovery-retry")?,
                ..recovery
            })
            .await?,
        recovered
    );
    let stale_recovery_token = FanActionToken::parse("3".repeat(64))?;
    insert_action_token(
        &pool,
        workspace_id,
        signup.fan_id.into_uuid(),
        "session",
        &stale_recovery_token,
    )
    .await?;

    let unsubscribe_token =
        outbox_token(&pool, workspace_id, "fan.confirmed", "unsubscribe_token").await?;
    let unsubscribe_token = FanActionToken::parse(unsubscribe_token)?;
    let unsubscribed = lifecycle
        .unsubscribe(workspace_id, &unsubscribe_token)
        .await?;
    assert_eq!(unsubscribed.status, FanStatus::Unsubscribed);
    assert_eq!(city_count(&pool, workspace_id).await?, 0);
    assert_eq!(
        lifecycle
            .confirm(&confirmation_command(
                workspace_id,
                stale_recovery_token,
                "stale-session-recovery"
            )?)
            .await,
        Err(RepositoryError::Conflict)
    );

    let replay = lifecycle
        .unsubscribe(workspace_id, &unsubscribe_token)
        .await?;
    assert_eq!(replay.status, FanStatus::Unsubscribed);
    assert_eq!(city_count(&pool, workspace_id).await?, 0);

    assert_late_confirmation_cannot_reverse_unsubscribe(
        &pool,
        &database,
        &lifecycle,
        workspace_id,
        &suffix,
    )
    .await?;

    pool.close().await;
    Ok(())
}

async fn assert_late_confirmation_cannot_reverse_unsubscribe(
    pool: &PgPool,
    database: &DatabaseConfig,
    lifecycle: &PostgresFanLifecycleRepository,
    workspace_id: WorkspaceId,
    suffix: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let identity_suffix = format!("late-{suffix}");
    let pending_repository = PostgresAcquisitionRepository::new(
        pool.clone(),
        WorkspaceSlug::parse(format!("lifecycle-{suffix}"))?,
        CountryCode::parse("PL")?,
        database,
        true,
        test_sensitive_response_codec(),
    );
    let pending = pending_repository
        .persist_fan_signup(&signup_command_with_key(
            workspace_id,
            &identity_suffix,
            "pending",
        )?)
        .await?;
    assert_eq!(pending.status, FanStatus::Pending);
    let stale_confirmation = FanActionToken::parse(
        outbox_token(
            pool,
            workspace_id,
            "fan.confirmation_requested",
            "confirmation_token",
        )
        .await?,
    )?;

    // Model a legacy transition that left a confirmation token live while the
    // fan became active. The lifecycle boundary must still refuse that token
    // after the fan explicitly unsubscribes.
    sqlx::query("UPDATE fans SET status = 'active' WHERE workspace_id = $1 AND id = $2")
        .bind(workspace_id.into_uuid())
        .bind(pending.fan_id.into_uuid())
        .execute(pool)
        .await?;
    let unsubscribe =
        FanActionToken::parse("1111111111111111111111111111111111111111111111111111111111111111")?;
    sqlx::query(
        r#"
        INSERT INTO fan_action_tokens (
            workspace_id, fan_id, purpose, token_hash, expires_at
        )
        VALUES ($1, $2, 'unsubscribe', digest($3, 'sha256'), now() + interval '1 day')
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(pending.fan_id.into_uuid())
    .bind(unsubscribe.as_str())
    .execute(pool)
    .await?;
    lifecycle.unsubscribe(workspace_id, &unsubscribe).await?;

    assert!(
        lifecycle
            .confirm(&confirmation_command(
                workspace_id,
                stale_confirmation,
                "late-confirmation"
            )?)
            .await
            .is_err(),
        "a confirmation issued before unsubscribe must never reactivate the fan"
    );
    let status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM fans WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(pending.fan_id.into_uuid())
    .fetch_one(pool)
    .await?;
    assert_eq!(status, "unsubscribed");
    Ok(())
}

fn test_database_config(url: String) -> DatabaseConfig {
    DatabaseConfig {
        url,
        max_connections: 8,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
    }
}

fn test_sensitive_response_codec() -> SensitiveResponseCodec {
    SensitiveResponseCodec::new(SensitiveResponseKey::derive_from_secret(
        b"lifecycle-integration-response-secret",
    ))
}

fn confirmation_command(
    workspace_id: WorkspaceId,
    token: FanActionToken,
    suffix: &str,
) -> Result<ConfirmFanCommand, Box<dyn std::error::Error>> {
    Ok(ConfirmFanCommand {
        workspace_id,
        token,
        idempotency_key: IdempotencyKey::parse(format!("confirm-{suffix}"))?,
        request_id: RequestId::parse(format!("request-{suffix}"))?,
    })
}

async fn insert_action_token(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    fan_id: uuid::Uuid,
    purpose: &str,
    token: &FanActionToken,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        r#"
        INSERT INTO fan_action_tokens (
            workspace_id, fan_id, purpose, token_hash, expires_at
        )
        VALUES ($1, $2, $3, digest($4, 'sha256'), now() + interval '1 day')
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .bind(purpose)
    .bind(token.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

fn signup_command(
    workspace_id: WorkspaceId,
    suffix: &str,
) -> Result<SignupFanCommand, Box<dyn std::error::Error>> {
    signup_command_with_key(workspace_id, suffix, suffix)
}

fn signup_command_with_key(
    workspace_id: WorkspaceId,
    identity_suffix: &str,
    key_suffix: &str,
) -> Result<SignupFanCommand, Box<dyn std::error::Error>> {
    let signup = FanSignup::new(FanSignupInput {
        workspace_id,
        email: NormalizedEmail::parse(format!("fan-{identity_suffix}@example.test"))?,
        display_name: Some("Test fan".to_owned()),
        city_slug: CitySlug::parse("wroclaw")?,
        locale: Some("pl-PL".to_owned()),
        campaign_id: None,
        visitor_id: None,
        claimed_referral_code: None,
        consent: MarketingConsent::new(true, "privacy-2026-07", "integration-test")?,
    })?;
    Ok(SignupFanCommand::new(
        IdempotencyKey::parse(format!("lifecycle-{key_suffix}"))?,
        RequestId::parse(format!("request-{key_suffix}"))?,
        signup,
    ))
}

async fn seed_workspace(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    slug: &WorkspaceSlug,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Lifecycle E2E')")
        .bind(workspace_id.into_uuid())
        .bind(slug.as_str())
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO cities (slug, name, country_code) VALUES ('wroclaw', 'Wrocław', 'PL') ON CONFLICT (country_code, slug) DO NOTHING",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO city_aggregates (workspace_id, city_id)
        SELECT $1, id FROM cities WHERE country_code = 'PL' AND slug = 'wroclaw'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .execute(pool)
    .await?;
    Ok(())
}

async fn outbox_token(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    event_type: &str,
    field: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let payload = sqlx::query_scalar::<_, Value>(
        "SELECT payload FROM outbox_events WHERE workspace_id = $1 AND event_type = $2 ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(workspace_id.into_uuid())
    .bind(event_type)
    .fetch_one(pool)
    .await?;
    Ok(payload[field]
        .as_str()
        .ok_or("token field must be a string")?
        .to_owned())
}

async fn city_count(
    pool: &PgPool,
    workspace_id: WorkspaceId,
) -> Result<i64, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT confirmed_fan_count FROM city_aggregates WHERE workspace_id = $1",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(pool)
    .await?)
}
