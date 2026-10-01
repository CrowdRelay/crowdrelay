#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn initial_signup_metadata_is_atomic_idempotent_and_not_anonymous_authority()
-> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_application::{ConfirmFanCommand, FanLifecycleRepository};
    use crowdrelay_domain::{FanActionToken, acquisition::SignupMetadata};
    use crowdrelay_infra::fan_lifecycle::PostgresFanLifecycleRepository;
    let (pool, url) = common::test_pool_with_url(TEST_DATABASE_URL_KEY).await?;
    let workspace = WorkspaceId::new();
    let suffix = workspace.into_uuid().simple().to_string();
    let slug = WorkspaceSlug::parse(format!("metadata-{suffix}"))?;
    let city = CitySlug::parse(format!("city-{suffix}"))?;
    seed_acquisition_scope(
        &pool,
        workspace,
        &slug,
        &city,
        CampaignId::new(),
        CampaignId::new(),
        SmartLinkId::new(),
    )
    .await?;
    let database = DatabaseConfig {
        url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(5),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(2),
    };
    let repository = PostgresAcquisitionRepository::new(
        pool.clone(),
        slug.clone(),
        CountryCode::parse("PL")?,
        &database,
        true,
        test_sensitive_response_codec(),
    );
    let base = signup_command(
        workspace,
        &format!("metadata-{suffix}@example.test"),
        &city,
        None,
        None,
        format!("first-{suffix}"),
        format!("request-{suffix}"),
    )?;
    let safe = SignupMetadata {
        nearby_gigs: Some((true, 150)),
        utm_campaign: Some("safe".into()),
        ..Default::default()
    };
    let first = SignupFanCommand::new(
        base.idempotency_key().clone(),
        base.request_id().clone(),
        base.signup().clone().with_initial_metadata(safe.clone())?,
    );
    let captured = repository.persist_fan_signup(&first).await?;
    let snapshot = || async {
        sqlx::query_as::<_, (bool, i32, String)>("SELECT pref.nearby_gigs_enabled,pref.radius_km,attr.utm_campaign FROM fan_location_preferences pref JOIN fan_ad_attribution attr ON attr.workspace_id=pref.workspace_id AND attr.fan_id=pref.fan_id WHERE pref.workspace_id=$1 AND pref.fan_id=$2")
        .bind(workspace.into_uuid()).bind(captured.fan_id.into_uuid()).fetch_one(&pool).await
    };
    assert_eq!(snapshot().await?, (true, 150, "safe".into()));
    let malicious = SignupMetadata {
        nearby_gigs: Some((false, 25)),
        utm_campaign: Some("injected".into()),
        ..Default::default()
    };
    let repeat = SignupFanCommand::new(
        IdempotencyKey::parse(format!("repeat-{suffix}"))?,
        base.request_id().clone(),
        base.signup()
            .clone()
            .with_initial_metadata(malicious.clone())?,
    );
    assert!(
        repository
            .persist_fan_signup(&repeat)
            .await?
            .fan_session_token
            .is_none()
    );
    assert_eq!(snapshot().await?, (true, 150, "safe".into()));
    let token: String = sqlx::query_scalar("SELECT payload->>'confirmation_token' FROM outbox_events WHERE workspace_id=$1 AND event_type='fan.confirmation_requested' ORDER BY created_at LIMIT 1")
        .bind(workspace.into_uuid()).fetch_one(&pool).await?;
    let lifecycle = PostgresFanLifecycleRepository::new(
        pool.clone(),
        slug,
        &database,
        test_sensitive_response_codec(),
    );
    lifecycle
        .confirm(&ConfirmFanCommand {
            workspace_id: workspace,
            token: FanActionToken::parse(token)?,
            idempotency_key: IdempotencyKey::parse(format!("confirm-{suffix}"))?,
            request_id: base.request_id().clone(),
        })
        .await?;
    let replay = first
        .signup()
        .clone()
        .with_signup_transport(Some("192.0.2.1".into()), Some("Different network".into()));
    let replay = SignupFanCommand::new(
        first.idempotency_key().clone(),
        first.request_id().clone(),
        replay,
    );
    assert_eq!(repository.persist_fan_signup(&replay).await?, captured);
    let active_repeat = SignupFanCommand::new(
        IdempotencyKey::parse(format!("active-{suffix}"))?,
        base.request_id().clone(),
        base.signup()
            .clone()
            .with_initial_metadata(malicious.clone())?,
    );
    assert!(
        repository
            .persist_fan_signup(&active_repeat)
            .await?
            .fan_session_token
            .is_none()
    );
    let conflicting = SignupFanCommand::new(
        first.idempotency_key().clone(),
        first.request_id().clone(),
        first.signup().clone().with_initial_metadata(malicious)?,
    );
    assert!(matches!(
        repository.persist_fan_signup(&conflicting).await,
        Err(RepositoryError::Conflict)
    ));
    assert_eq!(snapshot().await?, (true, 150, "safe".into()));
    Ok(())
}
