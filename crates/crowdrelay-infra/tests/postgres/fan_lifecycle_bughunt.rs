struct BughuntFan {
    pool: PgPool,
    workspace: WorkspaceId,
    suffix: String,
    acquisition: PostgresAcquisitionRepository,
    lifecycle: PostgresFanLifecycleRepository,
    confirmed: crowdrelay_domain::FanConfirmationResult,
    unsubscribe: FanActionToken,
}

async fn bughunt_token(
    pool: &PgPool,
    workspace: WorkspaceId,
    event: &str,
    field: &str,
) -> Result<FanActionToken, Box<dyn std::error::Error>> {
    let token: String = sqlx::query_scalar("SELECT payload->>$3 FROM outbox_events WHERE workspace_id=$1 AND event_type=$2 ORDER BY created_at DESC,id DESC LIMIT 1")
        .bind(workspace.into_uuid()).bind(event).bind(field).fetch_one(pool).await?;
    Ok(FanActionToken::parse(token)?)
}

async fn bughunt_active_fan() -> Result<BughuntFan, Box<dyn std::error::Error>> {
    let (pool, url) =
        common::test_pool_with_url("CROWDRELAY_FAN_LIFECYCLE_TEST_DATABASE_URL").await?;
    let workspace = WorkspaceId::new();
    let suffix = workspace.into_uuid().simple().to_string();
    let slug = WorkspaceSlug::parse(format!("bughunt-{suffix}"))?;
    seed_workspace(&pool, workspace, &slug).await?;
    let config = test_database_config(url);
    let acquisition = PostgresAcquisitionRepository::new(
        pool.clone(),
        slug.clone(),
        CountryCode::parse("PL")?,
        &config,
        true,
        test_sensitive_response_codec(),
    );
    let lifecycle = PostgresFanLifecycleRepository::new(
        pool.clone(),
        slug,
        &config,
        test_sensitive_response_codec(),
    );
    acquisition
        .persist_fan_signup(&signup_command(workspace, &suffix)?)
        .await?;
    let confirmed = lifecycle
        .confirm(&ConfirmFanCommand {
            workspace_id: workspace,
            token: bughunt_token(
                &pool,
                workspace,
                "fan.confirmation_requested",
                "confirmation_token",
            )
            .await?,
            idempotency_key: IdempotencyKey::parse(format!("confirm-{suffix}"))?,
            request_id: RequestId::parse(format!("request-{suffix}"))?,
        })
        .await?;
    let unsubscribe = bughunt_token(&pool, workspace, "fan.confirmed", "unsubscribe_token").await?;
    Ok(BughuntFan {
        pool,
        workspace,
        suffix,
        acquisition,
        lifecycle,
        confirmed,
        unsubscribe,
    })
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_FAN_LIFECYCLE_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn reused_unsubscribe_link_honors_new_withdrawal_after_resubscription()
-> Result<(), Box<dyn std::error::Error>> {
    let fan = bughunt_active_fan().await?;
    fan.lifecycle
        .unsubscribe(fan.workspace, &fan.unsubscribe)
        .await?;
    fan.acquisition
        .persist_fan_signup(&signup_command_with_key(
            fan.workspace,
            &fan.suffix,
            &format!("resub-{}", fan.suffix),
        )?)
        .await?;
    let confirmed = fan
        .lifecycle
        .confirm(&ConfirmFanCommand {
            workspace_id: fan.workspace,
            token: bughunt_token(
                &fan.pool,
                fan.workspace,
                "fan.confirmation_requested",
                "confirmation_token",
            )
            .await?,
            idempotency_key: IdempotencyKey::parse(format!("resub-confirm-{}", fan.suffix))?,
            request_id: RequestId::parse(format!("resub-request-{}", fan.suffix))?,
        })
        .await?;
    assert_eq!(confirmed.status, FanStatus::Active);
    assert_eq!(confirmed.fan_id, fan.confirmed.fan_id);
    assert_eq!(
        fan.lifecycle
            .unsubscribe(fan.workspace, &fan.unsubscribe)
            .await?
            .status,
        FanStatus::Unsubscribed
    );
    let actual: (String,bool) = sqlx::query_as("SELECT fan.status, (SELECT granted FROM fan_consents WHERE workspace_id=fan.workspace_id AND fan_id=fan.id AND purpose='marketing' ORDER BY recorded_at DESC,id DESC LIMIT 1) FROM fans fan WHERE workspace_id=$1 AND id=$2")
        .bind(fan.workspace.into_uuid()).bind(confirmed.fan_id.into_uuid()).fetch_one(&fan.pool).await?;
    assert_eq!(actual, ("unsubscribed".into(), false));
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fan_consents WHERE workspace_id=$1 AND fan_id=$2 AND NOT granted",
    )
    .bind(fan.workspace.into_uuid())
    .bind(confirmed.fan_id.into_uuid())
    .fetch_one(&fan.pool)
    .await?;
    fan.lifecycle
        .unsubscribe(fan.workspace, &fan.unsubscribe)
        .await?;
    let replay_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fan_consents WHERE workspace_id=$1 AND fan_id=$2 AND NOT granted",
    )
    .bind(fan.workspace.into_uuid())
    .bind(confirmed.fan_id.into_uuid())
    .fetch_one(&fan.pool)
    .await?;
    assert_eq!(count, replay_count);
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM fan_sessions WHERE workspace_id=$1 AND fan_id=$2 AND revoked_at IS NULL")
        .bind(fan.workspace.into_uuid()).bind(confirmed.fan_id.into_uuid()).fetch_one(&fan.pool).await?;
    assert_eq!(sessions, 0);
    Ok(())
}
