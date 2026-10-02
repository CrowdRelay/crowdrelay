// Referral acquisition tests split out of `lib_tests.rs` — the file ran over
// the source-size ratchet. `include!`d into `mod tests` so it shares that
// scope's helpers and imports.

    #[tokio::test]
    async fn referral_cookie_is_used_when_signup_body_has_no_code()
    -> Result<(), Box<dyn std::error::Error>> {
        let repository = Arc::new(TestRepository::happy()?);
        let repository_port: Arc<dyn AcquisitionRepository> = repository.clone();
        let app = test_router_with_state(state_with(
            repository_port,
            WorkspaceId::new(),
            Arc::new(RedirectCache::new()),
            Arc::new(|_event| {}),
        )?)?;

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/fans")
                    .header(CONTENT_TYPE, "application/json")
                    .header("idempotency-key", "signup-referral-cookie-0001")
                    .header(COOKIE, "crowdrelay_referral=Referrer_Code-123")
                    .body(Body::from(
                        r#"{"email":"cookie@example.com","city_slug":"wroclaw","consent":{"marketing":true,"policy_version":"privacy-v1"}}"#,
                    ))?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::CREATED);
        let commands = repository
            .signup_commands
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            commands[0]
                .signup()
                .claimed_referral_code()
                .map(ReferralCode::as_str),
            Some("Referrer_Code-123")
        );
        Ok(())
    }

    #[tokio::test]
    async fn referral_redirect_progress_and_redemption_routes_are_private()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace_id = WorkspaceId::new();
        let repository: Arc<dyn AcquisitionRepository> = Arc::new(TestRepository::happy()?);
        let app = test_router_with_state(state_with(
            repository,
            workspace_id,
            Arc::new(RedirectCache::new()),
            Arc::new(|_event| {}),
        )?)?;

        let redirect = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/r/Fan_Code-123")
                    .header(USER_AGENT, "Mozilla/5.0 CrowdRelay-Test-Browser")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(redirect.status(), StatusCode::FOUND);
        assert_eq!(redirect.headers()[LOCATION], "http://localhost:4321/join");
        let cookies = redirect
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .map(|value| value.to_str())
            .collect::<Result<Vec<_>, _>>()?;
        assert!(
            cookies
                .iter()
                .any(|cookie| cookie.contains("crowdrelay_referral=Fan_Code-123"))
        );
        assert!(
            cookies
                .iter()
                .any(|cookie| cookie.contains("crowdrelay_attribution="))
        );
        assert_eq!(redirect.headers()["cache-control"], "private, no-store");

        let unauthorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/me/referral")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let session = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let progress = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/me/referral")
                    .header(COOKIE, format!("crowdrelay_fan={session}"))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(progress.status(), StatusCode::OK);
        assert_eq!(progress.headers()["cache-control"], "private, no-store");

        let unauthorized_redeem = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/commerce/coupons/redeem")
                    .header(CONTENT_TYPE, "application/json")
                    .header("idempotency-key", "coupon-redeem-test-0001")
                    .body(Body::from(
                        r#"{"code":"VIRYA-ABC12345","order_reference":"order-1"}"#,
                    ))?,
            )
            .await?;
        assert_eq!(unauthorized_redeem.status(), StatusCode::UNAUTHORIZED);

        let redeemed = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/commerce/coupons/redeem")
                    .header(CONTENT_TYPE, "application/json")
                    .header("authorization", "Bearer test-commerce-api-key-1234567890")
                    .header("idempotency-key", "coupon-redeem-test-0001")
                    .body(Body::from(
                        r#"{"code":"VIRYA-ABC12345","order_reference":"order-1"}"#,
                    ))?,
            )
            .await?;
        assert_eq!(redeemed.status(), StatusCode::OK);
        assert_eq!(redeemed.headers()["cache-control"], "private, no-store");
        Ok(())
    }

    #[tokio::test]
    async fn contextual_referral_lands_on_a_real_show_and_rejects_invented_context()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace_id = WorkspaceId::new();
        let cache = Arc::new(EventCache::new());
        cache.replace_for_workspace(
            workspace_id,
            [PublicEvent {
                id: crowdrelay_domain::EventId::new(),
                slug: crowdrelay_domain::EventSlug::parse("test-show")?,
                title: "Test Show".to_owned(),
                description: None,
                city: Some(crowdrelay_domain::EventCity {
                    id: crowdrelay_domain::CityId::new(),
                    slug: "wroclaw".to_owned(),
                    name: "Wrocław".to_owned(),
                    country_code: "PL".to_owned(),
                    region: None,
                }),
                venue: None,
                venue_address: None,
                timezone: "Europe/Warsaw".to_owned(),
                starts_at: time::OffsetDateTime::now_utc() + time::Duration::days(7),
                doors_at: None,
                ends_at: None,
                ticket_url: None,
                listen_url: None,
                image_url: None,
                trailer_url: None,
                external_event_url: None,
                festival_name: None,
                acts: Vec::new(),
                updated_at: time::OffsetDateTime::now_utc(),
            }],
        )?;

        let repository: Arc<dyn AcquisitionRepository> = Arc::new(TestRepository::happy()?);
        let app = test_router_with_state(state_with_event_state(
            repository,
            workspace_id,
            Arc::new(RedirectCache::new()),
            Arc::new(|_event| {}),
            event_state_with_cache(workspace_id, cache),
            None,
        )?)?;

        let real = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/r/Fan_Code-123?event=test-show&lang=pl")
                    .header(USER_AGENT, "Mozilla/5.0 CrowdRelay-Test-Browser")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(real.status(), StatusCode::FOUND);
        assert_eq!(
            real.headers()[LOCATION],
            "http://localhost:4321/pl/live/test-show/"
        );

        let invented = app
            .oneshot(
                Request::builder()
                    .uri("/v1/r/Fan_Code-123?event=made-up-show&lang=pl")
                    .header(USER_AGENT, "Mozilla/5.0 CrowdRelay-Test-Browser")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(invented.status(), StatusCode::FOUND);
        assert_eq!(invented.headers()[LOCATION], "http://localhost:4321/join");
        Ok(())
    }

