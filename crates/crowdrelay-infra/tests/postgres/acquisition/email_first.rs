#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn email_first_capture_confirms_without_a_city_and_preserves_attribution()
-> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_application::{ConfirmFanCommand, FanLifecycleRepository};
    use crowdrelay_domain::FanActionToken;
    use crowdrelay_infra::fan_lifecycle::PostgresFanLifecycleRepository;

    let (pool, url) = common::test_pool_with_url(TEST_DATABASE_URL_KEY).await?;
    let workspace = WorkspaceId::new();
    let suffix = workspace.into_uuid().simple().to_string();
    let slug = WorkspaceSlug::parse(format!("email-first-{suffix}"))?;
    let city = CitySlug::parse(format!("city-{suffix}"))?;
    let campaign = CampaignId::new();
    let link = SmartLinkId::new();
    seed_acquisition_scope(&pool, workspace, &slug, &city, campaign, CampaignId::new(), link).await?;
    sqlx::query("UPDATE smart_links SET channel_source='instagram', channel_creative='shows' WHERE workspace_id=$1 AND id=$2")
        .bind(workspace.into_uuid()).bind(link.into_uuid()).execute(&pool).await?;
    let database = DatabaseConfig {
        url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(5),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(2),
    };
    let repository = PostgresAcquisitionRepository::new(
        pool.clone(), slug.clone(), CountryCode::parse("PL")?, &database, true,
        test_sensitive_response_codec(),
    );
    let referrer = FanId::new();
    let code = ReferralCode::parse(format!("Ref_{suffix}"))?;
    sqlx::query("INSERT INTO fans (id,workspace_id,normalized_email,status) VALUES ($1,$2,$3,'active')")
        .bind(referrer.into_uuid()).bind(workspace.into_uuid())
        .bind(format!("referrer-{suffix}@example.test")).execute(&pool).await?;
    sqlx::query("INSERT INTO referral_codes (workspace_id,fan_id,code) VALUES ($1,$2,$3)")
        .bind(workspace.into_uuid()).bind(referrer.into_uuid()).bind(code.as_str())
        .execute(&pool).await?;
    let visitor = VisitorId::new();
    let resolved = ResolvedSmartLink::new(
        link, workspace, Some(campaign), SmartLinkSlug::parse("infra-test")?,
        DestinationUrl::parse("https://example.test/signal?offer=shows")?, 1,
        Some("instagram".to_owned()), None,
    )?;
    repository.persist_click_batch(&[ClickEvent::from_link(
        &resolved, Some(visitor), None, OffsetDateTime::now_utc(),
    )?]).await?;
    let command = SignupFanCommand::new(
        IdempotencyKey::parse(format!("capture-{suffix}"))?,
        RequestId::parse(format!("request-{suffix}"))?,
        FanSignup::new(FanSignupInput {
            workspace_id: workspace,
            email: NormalizedEmail::parse(format!("capture-{suffix}@example.test"))?,
            display_name: None,
            city_slug: None,
            locale: Some("pl-PL".to_owned()),
            campaign_id: Some(campaign),
            visitor_id: Some(visitor),
            claimed_referral_code: Some(code),
            consent: MarketingConsent::new(true, "v1", "public_signup")?,
        })?,
    );
    let captured = repository.persist_fan_signup(&command).await?;
    assert!(captured.created);
    assert_eq!(captured.status, FanStatus::Pending);
    assert!(captured.email_queued);
    assert_eq!(repository.persist_fan_signup(&command).await?, captured);
    let token: String = sqlx::query_scalar(
        "SELECT payload->>'confirmation_token' FROM outbox_events WHERE workspace_id=$1 AND event_type='fan.confirmation_requested'",
    ).bind(workspace.into_uuid()).fetch_one(&pool).await?;
    let lifecycle = PostgresFanLifecycleRepository::new(
        pool.clone(), slug, &database, test_sensitive_response_codec(),
    );
    let confirmed = lifecycle.confirm(&ConfirmFanCommand {
        workspace_id: workspace,
        token: FanActionToken::parse(token)?,
        idempotency_key: IdempotencyKey::parse(format!("confirm-{suffix}"))?,
        request_id: RequestId::parse(format!("confirm-request-{suffix}"))?,
    }).await?;
    assert_eq!(confirmed.status, FanStatus::Active);
    let city_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fan_city_interests WHERE workspace_id=$1 AND fan_id=$2",
    ).bind(workspace.into_uuid()).bind(captured.fan_id.into_uuid()).fetch_one(&pool).await?;
    assert_eq!(city_count, 0, "email-first capture never invents a city");
    let acquisition: (i64, Option<Uuid>, Option<Uuid>) = sqlx::query_as(
        "SELECT count(*), max(anonymous_visitor_id::text)::uuid, max(campaign_id::text)::uuid FROM fan_acquisition_events WHERE workspace_id=$1 AND fan_id=$2",
    ).bind(workspace.into_uuid()).bind(captured.fan_id.into_uuid()).fetch_one(&pool).await?;
    assert_eq!(acquisition, (1, Some(visitor.into_uuid()), Some(campaign.into_uuid())));
    let conversions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fan_provenance_events WHERE workspace_id=$1 AND fan_id=$2 AND channel='instagram' AND attribution_method='last_tracked_click'",
    ).bind(workspace.into_uuid()).bind(captured.fan_id.into_uuid()).fetch_one(&pool).await?;
    assert_eq!(conversions, 1);
    let referrals: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM referral_attributions WHERE workspace_id=$1 AND referred_fan_id=$2 AND referrer_fan_id=$3 AND status='qualified'",
    ).bind(workspace.into_uuid()).bind(captured.fan_id.into_uuid()).bind(referrer.into_uuid())
        .fetch_one(&pool).await?;
    assert_eq!(referrals, 1);
    Ok(())
}
