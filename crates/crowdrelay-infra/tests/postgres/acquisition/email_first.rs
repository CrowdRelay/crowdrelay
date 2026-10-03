#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn email_first_capture_confirms_without_a_city_and_preserves_attribution()
-> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_application::{ConfirmFanCommand, FanLifecycleRepository};
    use crowdrelay_application::autopilot::{
        AutopilotMeasurementKind, AutopilotMeasurementRepository, ClaimedAutopilotMeasurement,
    };
    use crowdrelay_domain::FanActionToken;
    use crowdrelay_domain::ids::{AutopilotActionId, AutopilotMeasurementId};
    use crowdrelay_infra::{
        autopilot::PostgresAutopilotRepository,
        fan_lifecycle::PostgresFanLifecycleRepository,
    };

    let (pool, url) = common::test_pool_with_url(TEST_DATABASE_URL_KEY).await?;
    let workspace = WorkspaceId::new();
    let suffix = workspace.into_uuid().simple().to_string();
    let slug = WorkspaceSlug::parse(format!("email-first-{suffix}"))?;
    let city = CitySlug::parse(format!("city-{suffix}"))?;
    let campaign = CampaignId::new();
    let link = SmartLinkId::new();
    seed_acquisition_scope(&pool, workspace, &slug, &city, campaign, CampaignId::new(), link).await?;

    // Give this public acquisition rail a real action owner. The action exists
    // first because smart_links.action_id is workspace-scoped FK evidence, not
    // an arbitrary label.
    let decision_id = Uuid::now_v7();
    let action_id = Uuid::now_v7();
    let action_finished_at = OffsetDateTime::now_utc() - time::Duration::minutes(5);
    sqlx::query(
        "INSERT INTO autopilot_decisions(
             id,workspace_id,decision_key,context,subject_kind,subject_id,
             decision_kind,confidence_basis_points,disposition,reason,
             input_snapshot,policy_snapshot,recommendation,trace_id
         ) VALUES(
             $1,$2,$3,'growth_metrics','target_community',$4,
             'auto_execute',9000,'auto_execute','signal e2e proof',
             '{}'::jsonb,'{}'::jsonb,'{}'::jsonb,gen_random_uuid()
         )",
    )
    .bind(decision_id)
    .bind(workspace.into_uuid())
    .bind(format!("signal-e2e-{suffix}"))
    .bind(Uuid::now_v7())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO autopilot_actions(
             id,workspace_id,decision_id,context,action_kind,subject_kind,subject_id,
             idempotency_key,payload,status,action_class,finished_at
         ) VALUES(
             $1,$2,$3,'growth_metrics','agent.run.request','target_community',$4,
             $5,'{}'::jsonb,'succeeded','third_party',$6
         )",
    )
    .bind(action_id)
    .bind(workspace.into_uuid())
    .bind(decision_id)
    .bind(Uuid::now_v7())
    .bind(format!("signal-e2e-action-{suffix}"))
    .bind(action_finished_at)
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE smart_links
         SET channel_source='instagram',
             channel_creative='shows',
             action_id=$3
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(workspace.into_uuid())
    .bind(link.into_uuid())
    .bind(action_id)
    .execute(&pool)
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

    let provenance_action: Option<Uuid> = sqlx::query_scalar(
        "SELECT action_id
         FROM fan_provenance_events
         WHERE workspace_id=$1 AND fan_id=$2
           AND event_kind='conversion'
           AND attribution_method='last_tracked_click'
         ORDER BY occurred_at DESC,id DESC
         LIMIT 1",
    )
    .bind(workspace.into_uuid())
    .bind(captured.fan_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        provenance_action,
        Some(action_id),
        "the real click -> signup writer must carry the exact action into canonical conversion provenance"
    );

    // The same real fan now activates Signal. This is not a workspace counter:
    // the measurement below is only allowed to see canonical people whose
    // conversion provenance belongs to this exact action and whose endpoint
    // appeared after that conversion.
    sqlx::query(
        "INSERT INTO fan_push_endpoints(
             workspace_id,fan_id,installation_id,transport,endpoint_address,
             active,created_at,last_seen_at
         ) VALUES($1,$2,$3,'android_fcm',$4,true,now(),now())",
    )
    .bind(workspace.into_uuid())
    .bind(captured.fan_id.into_uuid())
    .bind(format!("signal-e2e-install-{suffix}"))
    .bind(format!("signal-e2e-endpoint-{suffix}"))
    .execute(&pool)
    .await?;

    let autopilot = PostgresAutopilotRepository::new(pool.clone(), &database);
    let measurement = ClaimedAutopilotMeasurement {
        id: AutopilotMeasurementId::from(Uuid::now_v7()),
        action_id: AutopilotActionId::from(action_id),
        kind: AutopilotMeasurementKind::AgentRunSignalInstalls7d,
        subject_id: action_id,
        baseline_value: 0.0,
        action_finished_at,
        due_at: OffsetDateTime::now_utc(),
        attempt_number: 1,
    };
    assert_eq!(
        autopilot
            .observe_measurement(workspace, &measurement, OffsetDateTime::now_utc())
            .await?,
        1.0,
        "real click -> signup -> confirmation -> Signal activation must prove exactly one action-owned canonical fan"
    );
    Ok(())
}
