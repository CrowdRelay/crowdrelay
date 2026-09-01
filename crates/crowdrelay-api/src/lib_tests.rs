#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use async_trait::async_trait;
    use axum::{
        body::{Body, to_bytes},
        http::{
            Request, StatusCode,
            header::{
                AUTHORIZATION, CONTENT_TYPE, COOKIE, ETAG, IF_NONE_MATCH, LOCATION, REFERER,
                SET_COOKIE,
            },
        },
    };
    use crowdrelay_application::{
        AcquisitionRepository, AdmissionRepository, ClaimAdmissionPass, ConfirmFan,
        ConfirmFanCommand, EventCache, EventRepository, FanLifecycleRepository, IssueAdmissionPass,
        ListCities, ListFanEventInterests, LoadAdmissionPass, LoadReferralProgress,
        RedeemAdmissionPass, RedeemCoupon, RedeemCouponCommand, RedirectCache, ReferralRepository,
        RegisterEventInterest, RegisterEventInterestCommand, RepositoryError, ResolveReferralCode,
        RevokeAdmissionPass, SignupFan, SignupFanCommand, UnsubscribeFan, UpsertSmartLinkCommand,
        UpsertedSmartLink,
    };
    use crowdrelay_domain::{
        AdmissionPassClaimed, AdmissionPassIssued, AdmissionPassView, AdmissionRedemptionResult,
        CampaignId, CityId, CitySignal, CitySlug, ClickEvent, CountryCode, CouponRedemptionResult,
        CouponStatus, DestinationUrl, EventAction, EventInterestResult, FanActionToken,
        FanConfirmationResult, FanEventInterest, FanId, FanSessionToken, FanSignupResult,
        FanStatus, FanUnsubscribeResult, PassSessionToken, PublicEvent, ReferralCode,
        ReferralProgress, ResolvedSmartLink, SmartLinkId, SmartLinkSlug, WorkspaceId,
        WorkspaceSlug,
    };
    use crowdrelay_infra::autopilot::PostgresAutopilotRepository;
    use serde_json::Value;
    use sha2::Digest;
    use sqlx::postgres::PgPoolOptions;
    use tower::ServiceExt;
    use url::Url;

    use crate::{AdmissionStateArgs, acquisition};

    use super::{
        AcquisitionState, AdmissionState, AppState, ClickSubmitter, ConcertQrState,
        EventActionMetricsSnapshot, EventState, FanLifecycleState, HttpConfig, OpsState,
        ReferralState, TicketingState, X_REQUEST_ID, router,
    };

    struct TestRepository {
        signup_result: Result<FanSignupResult, RepositoryError>,
        cities_result: Result<Vec<CitySignal>, RepositoryError>,
        signup_commands: Mutex<Vec<SignupFanCommand>>,
    }

    impl TestRepository {
        fn unavailable() -> Self {
            Self {
                signup_result: Err(RepositoryError::Unavailable),
                cities_result: Err(RepositoryError::Unavailable),
                signup_commands: Mutex::new(Vec::new()),
            }
        }

        fn happy() -> Result<Self, Box<dyn std::error::Error>> {
            Ok(Self {
                signup_result: Ok(FanSignupResult {
                    fan_id: FanId::new(),
                    status: FanStatus::Active,
                    referral_code: Some(ReferralCode::parse("Fan_Code-123")?),
                    fan_session_token: Some(FanSessionToken::parse(
                        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    )?),
                    confirmation_required: false,
                    created: true,
                    email_kind: None,
                    email_queued: false,
                    retry_after_seconds: None,
                }),
                cities_result: Ok(vec![CitySignal::new(
                    CityId::new(),
                    CitySlug::parse("wroclaw")?,
                    "Wrocław",
                    CountryCode::parse("PL")?,
                    42,
                )?]),
                signup_commands: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl AcquisitionRepository for TestRepository {
        async fn resolve_workspace(
            &self,
            _slug: &WorkspaceSlug,
        ) -> Result<Option<WorkspaceId>, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn load_active_smart_links(&self) -> Result<Vec<ResolvedSmartLink>, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn persist_click_batch(&self, _clicks: &[ClickEvent]) -> Result<(), RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn persist_fan_signup(
            &self,
            command: &SignupFanCommand,
        ) -> Result<FanSignupResult, RepositoryError> {
            self.signup_commands
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(command.clone());
            self.signup_result.clone()
        }

        async fn list_city_signals(
            &self,
            _workspace_id: WorkspaceId,
            _limit: u32,
        ) -> Result<Vec<CitySignal>, RepositoryError> {
            self.cities_result.clone()
        }

        async fn upsert_smart_link<'a>(
            &self,
            _command: &UpsertSmartLinkCommand<'a>,
        ) -> Result<UpsertedSmartLink, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn list_smart_links(
            &self,
            _workspace_id: WorkspaceId,
        ) -> Result<Vec<UpsertedSmartLink>, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn load_or_create_fan_referral_code(
            &self,
            _workspace_id: WorkspaceId,
            _fan_id: FanId,
        ) -> Result<ReferralCode, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }
    }

    struct TestReferralRepository;

    #[async_trait]
    impl ReferralRepository for TestReferralRepository {
        async fn referral_code_is_active(
            &self,
            _workspace_id: WorkspaceId,
            _code: &ReferralCode,
        ) -> Result<bool, RepositoryError> {
            Ok(true)
        }

        async fn load_referral_progress(
            &self,
            _workspace_id: WorkspaceId,
            _session_token: &FanSessionToken,
        ) -> Result<ReferralProgress, RepositoryError> {
            Ok(ReferralProgress {
                referral_code: ReferralCode::parse("Fan_Code-123")
                    .map_err(|_| RepositoryError::Unavailable)?,
                qualified_referrals: 3,
                pending_referrals: 0,
                next_reward_threshold: Some(5),
                draw_entries: Vec::new(),
                coupons: Vec::new(),
                physical_rewards: Vec::new(),
            })
        }

        async fn redeem_coupon(
            &self,
            _command: &RedeemCouponCommand,
        ) -> Result<CouponRedemptionResult, RepositoryError> {
            Ok(CouponRedemptionResult {
                coupon_id: crowdrelay_domain::MerchCouponId::new(),
                reward_grant_id: crowdrelay_domain::RewardGrantId::new(),
                status: CouponStatus::Redeemed,
                used_count: 1,
                max_uses: 1,
                redeemed_at: time::OffsetDateTime::UNIX_EPOCH,
            })
        }
    }

    struct TestEventRepository;

    #[async_trait]
    impl EventRepository for TestEventRepository {
        async fn load_published_events(&self) -> Result<Vec<PublicEvent>, RepositoryError> {
            Ok(Vec::new())
        }

        async fn persist_event_action(
            &self,
            _actions: &[EventAction],
        ) -> Result<(), RepositoryError> {
            Ok(())
        }

        async fn register_interest(
            &self,
            _command: &RegisterEventInterestCommand,
        ) -> Result<EventInterestResult, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn list_fan_interests(
            &self,
            _workspace_id: WorkspaceId,
            _session_token: &FanSessionToken,
            _limit: u32,
        ) -> Result<Vec<FanEventInterest>, RepositoryError> {
            Ok(Vec::new())
        }
    }

    struct TestAdmissionRepository;

    #[async_trait]
    impl AdmissionRepository for TestAdmissionRepository {
        async fn issue_pass(
            &self,
            _command: &crowdrelay_application::IssueAdmissionPassCommand,
        ) -> Result<AdmissionPassIssued, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn claim_pass(
            &self,
            _command: &crowdrelay_application::ClaimAdmissionPassCommand,
        ) -> Result<AdmissionPassClaimed, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn load_pass(
            &self,
            _workspace_id: WorkspaceId,
            _session: &PassSessionToken,
        ) -> Result<AdmissionPassView, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn redeem_pass(
            &self,
            _command: &crowdrelay_application::RedeemAdmissionPassCommand,
        ) -> Result<AdmissionRedemptionResult, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn revoke_pass(
            &self,
            _command: &crowdrelay_application::RevokeAdmissionPassCommand,
        ) -> Result<AdmissionPassView, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }
    }

    struct TestFanLifecycleRepository;

    #[async_trait]
    impl FanLifecycleRepository for TestFanLifecycleRepository {
        async fn confirm(
            &self,
            _command: &ConfirmFanCommand,
        ) -> Result<FanConfirmationResult, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

        async fn unsubscribe(
            &self,
            _workspace_id: WorkspaceId,
            _token: &FanActionToken,
        ) -> Result<FanUnsubscribeResult, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }
    }

    fn admission_state(workspace_id: WorkspaceId) -> AdmissionState {
        let repository: Arc<dyn AdmissionRepository> = Arc::new(TestAdmissionRepository);
        AdmissionState::new(AdmissionStateArgs {
            workspace_id,
            issue_pass: IssueAdmissionPass::new(Arc::clone(&repository)),
            claim_pass: ClaimAdmissionPass::new(Arc::clone(&repository)),
            load_pass: LoadAdmissionPass::new(Arc::clone(&repository)),
            redeem_pass: RedeemAdmissionPass::new(Arc::clone(&repository)),
            revoke_pass: RevokeAdmissionPass::new(repository),
            qr_signing_key: None,
            qr_ttl: Duration::from_secs(30),
            secure_cookies: false,
        })
    }

    fn fan_lifecycle_state(
        workspace_id: WorkspaceId,
    ) -> Result<FanLifecycleState, Box<dyn std::error::Error>> {
        let repository: Arc<dyn FanLifecycleRepository> = Arc::new(TestFanLifecycleRepository);
        Ok(FanLifecycleState::new(
            workspace_id,
            ConfirmFan::new(Arc::clone(&repository)),
            UnsubscribeFan::new(repository),
            Url::parse("http://localhost:4321")?,
            false,
        ))
    }

    fn event_state(workspace_id: WorkspaceId) -> EventState {
        let repository: Arc<dyn EventRepository> = Arc::new(TestEventRepository);
        EventState::new(
            workspace_id,
            Arc::new(EventCache::new()),
            RegisterEventInterest::new(Arc::clone(&repository)),
            ListFanEventInterests::new(repository),
            Arc::new(|_action| {}),
            Arc::new(EventActionMetricsSnapshot::default),
        )
    }

    fn referral_state(
        workspace_id: WorkspaceId,
    ) -> Result<ReferralState, Box<dyn std::error::Error>> {
        let repository: Arc<dyn ReferralRepository> = Arc::new(TestReferralRepository);
        Ok(ReferralState::new(
            workspace_id,
            ResolveReferralCode::new(Arc::clone(&repository)),
            LoadReferralProgress::new(Arc::clone(&repository)),
            RedeemCoupon::new(repository),
            Url::parse("http://localhost:4321")?,
            false,
        ))
    }

    fn acquisition_state(
        repository: Arc<dyn AcquisitionRepository>,
        workspace_id: WorkspaceId,
        redirect_cache: Arc<RedirectCache>,
        click_submitter: ClickSubmitter,
    ) -> Result<AcquisitionState, Box<dyn std::error::Error>> {
        Ok(AcquisitionState::new(acquisition::AcquisitionStateArgs {
            workspace_id,
            redirect_cache,
            signup_fan: SignupFan::new(Arc::clone(&repository)),
            list_cities: ListCities::new(Arc::clone(&repository)),
            click_submitter,
            click_metrics_reader: Arc::new(super::ClickMetricsSnapshot::default),
            public_site_base_url: Url::parse("http://localhost:4321")?,
            secure_cookies: false,
            acquisition_repository: repository,
        }))
    }

    fn state_with(
        repository: Arc<dyn AcquisitionRepository>,
        workspace_id: WorkspaceId,
        redirect_cache: Arc<RedirectCache>,
        click_submitter: ClickSubmitter,
    ) -> Result<AppState, Box<dyn std::error::Error>> {
        let database = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_millis(50))
            .connect_lazy("postgres://crowdrelay:crowdrelay@127.0.0.1:1/crowdrelay")?;

        let concert_qr = ConcertQrState::new(workspace_id, database.clone(), None);
        let ticketing = TicketingState::new(
            workspace_id,
            database.clone(),
            Duration::from_millis(50),
            Some(sha2::Sha256::digest(b"test-admin-api-key-123456789012").into()),
            Some(sha2::Sha256::digest(b"test-staff-api-key-123456789012").into()),
            Some(sha2::Sha256::digest(b"test-previous-admin-key-1234567890").into()),
            Some(sha2::Sha256::digest(b"test-previous-staff-key-1234567890").into()),
            Some(sha2::Sha256::digest(b"test-commerce-api-key-1234567890").into()),
            None,
            Some([7_u8; 32]),
        );
        let ops = OpsState::new(workspace_id, database.clone(), Duration::from_millis(50));
        let autopilot = PostgresAutopilotRepository::new_with_timeouts(
            database.clone(),
            Duration::from_millis(50),
        );
        Ok(AppState::new(
            database,
            Duration::from_millis(50),
            acquisition_state(repository, workspace_id, redirect_cache, click_submitter)?,
            referral_state(workspace_id)?,
            event_state(workspace_id),
            admission_state(workspace_id),
            concert_qr,
            fan_lifecycle_state(workspace_id)?,
            ticketing,
            Some(sha2::Sha256::digest(b"test-area-management-key-1234567890").into()),
            Some(sha2::Sha256::digest(b"test-control-plane-key-123456789012").into()),
            None,
            None,
            ops,
            autopilot,
            false,
            crate::PushPublicState {
                runtime_enabled: false,
                web_push_vapid_public_key: None,
                fcm_project_id: None,
            },
            crate::tenant::TenantProfile {
                slug: "test".to_owned(),
                display_name: "Test".to_owned(),
                palette: crate::tenant::TenantPalette::default(),
                products: crate::tenant::TenantProducts {
                    crowdrelay: true,
                    signal: true,
                    synesthesia: false,
                },
                regional: crate::tenant::TenantRegionalProfile {
                    country_code: "US".to_owned(),
                    region: "us".to_owned(),
                    locale: "en-US".to_owned(),
                    timezone: "America/New_York".to_owned(),
                    currency: "USD".to_owned(),
                    date_format: "mdy".to_owned(),
                    number_format: "dot_decimal".to_owned(),
                    data_region: Some("us".to_owned()),
                },
                regional_provenance: crate::tenant::TenantRegionalProvenance {
                    country_code: crate::tenant::RegionalSource::TenantProfile,
                    region: crate::tenant::RegionalSource::TenantProfile,
                    locale: crate::tenant::RegionalSource::TenantProfile,
                    timezone: crate::tenant::RegionalSource::TenantProfile,
                    currency: crate::tenant::RegionalSource::TenantProfile,
                    date_format: crate::tenant::RegionalSource::TenantProfile,
                    number_format: crate::tenant::RegionalSource::TenantProfile,
                    data_region: crate::tenant::RegionalSource::TenantProfile,
                },
            },
            crowdrelay_infra::sensitive_response::SensitiveResponseKey::derive_from_secret(
                b"test-encryption-key",
            ),
            crowdrelay_infra::provider_verification::ProviderVerifiers::new(
                None,
                None,
                None,
                reqwest::Client::new(),
            ),
        ))
    }

    fn unavailable_state() -> Result<AppState, Box<dyn std::error::Error>> {
        let repository: Arc<dyn AcquisitionRepository> = Arc::new(TestRepository::unavailable());
        state_with(
            repository,
            WorkspaceId::new(),
            Arc::new(RedirectCache::new()),
            Arc::new(|_event| {}),
        )
    }

    fn test_router() -> Result<axum::Router, Box<dyn std::error::Error>> {
        Ok(router(
            unavailable_state()?,
            HttpConfig::new(["http://localhost:4321".to_owned()])?,
        ))
    }

    fn test_router_with_state(state: AppState) -> Result<axum::Router, Box<dyn std::error::Error>> {
        Ok(router(
            state,
            HttpConfig::new(["http://localhost:4321".to_owned()])?,
        ))
    }

    #[tokio::test]
    async fn liveness_does_not_depend_on_database() -> Result<(), Box<dyn std::error::Error>> {
        let response = test_router()?
            .oneshot(Request::builder().uri("/health/live").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(response.headers().contains_key(&X_REQUEST_ID));
        Ok(())
    }

    #[tokio::test]
    async fn versioned_liveness_contract_is_available() -> Result<(), Box<dyn std::error::Error>> {
        let response = test_router()?
            .oneshot(
                Request::builder()
                    .uri("/v1/health/live")
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    async fn prometheus_endpoint_fails_closed_when_ops_snapshot_is_unavailable()
    -> Result<(), Box<dyn std::error::Error>> {
        let response = test_router()?
            .oneshot(Request::builder().uri("/metrics").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(
            response.headers()[CONTENT_TYPE],
            "text/plain; version=0.0.4; charset=utf-8"
        );
        let body = to_bytes(response.into_body(), 16 * 1024).await?;
        let body = std::str::from_utf8(&body)?;
        assert!(body.contains("crowdrelay_ops_metrics_snapshot_available 0"));
        assert!(!body.contains("crowdrelay_outbox_pending 0"));
        Ok(())
    }

    #[tokio::test]
    async fn replaces_client_supplied_request_id() -> Result<(), Box<dyn std::error::Error>> {
        let response = test_router()?
            .oneshot(
                Request::builder()
                    .uri("/v1/health/live")
                    .header(&X_REQUEST_ID, "client-controlled-id")
                    .body(Body::empty())?,
            )
            .await?;

        let request_id = response.headers()[&X_REQUEST_ID].to_str()?;
        assert_ne!(request_id, "client-controlled-id");
        assert_eq!(request_id.len(), 36);
        Ok(())
    }

    #[tokio::test]
    async fn cors_allows_credentials_only_for_configured_origin()
    -> Result<(), Box<dyn std::error::Error>> {
        let response = test_router()?
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/v1/health/live")
                    .header("origin", "http://localhost:4321")
                    .header("access-control-request-method", "GET")
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(
            response.headers()["access-control-allow-origin"],
            "http://localhost:4321"
        );
        assert_eq!(
            response.headers()["access-control-allow-credentials"],
            "true"
        );
        Ok(())
    }

    #[tokio::test]
    async fn readiness_returns_problem_details_when_database_is_down()
    -> Result<(), Box<dyn std::error::Error>> {
        let response = test_router()?
            .oneshot(
                Request::builder()
                    .uri("/v1/health/ready")
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[CONTENT_TYPE], "application/problem+json");

        let response_request_id = response.headers()[&X_REQUEST_ID].to_str()?.to_owned();
        let body = to_bytes(response.into_body(), 16 * 1024).await?;
        let problem: Value = serde_json::from_slice(&body)?;

        assert_eq!(problem["status"], 503);
        assert_eq!(problem["request_id"], response_request_id);
        Ok(())
    }

    #[tokio::test]
    async fn redirect_uses_only_the_cache_and_enqueues_anonymous_click()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace_id = WorkspaceId::new();
        let campaign_id = CampaignId::new();
        let link = ResolvedSmartLink::new(
            SmartLinkId::new(),
            workspace_id,
            Some(campaign_id),
            SmartLinkSlug::parse("tour-2026")?,
            DestinationUrl::parse("https://virya.music/join")?,
            1,
        )?;
        let cache = Arc::new(RedirectCache::new());
        cache.replace([link.clone()])?;
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let click_capture = Arc::clone(&clicks);
        let repository: Arc<dyn AcquisitionRepository> = Arc::new(TestRepository::unavailable());
        let app = test_router_with_state(state_with(
            repository,
            workspace_id,
            cache,
            Arc::new(move |event| {
                click_capture
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(event)
            }),
        )?)?;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/go/tour-2026")
                    .header(REFERER, "https://social.example/post/123")
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(response.headers()[LOCATION], "https://virya.music/join");
        assert_eq!(response.headers()["cache-control"], "private, no-store");
        let cookie = response.headers()[SET_COOKIE].to_str()?;
        assert!(cookie.contains("crowdrelay_attribution="));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(!cookie.contains("; Secure"));

        let clicks = clicks.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(clicks.len(), 1);
        assert_eq!(clicks[0].smart_link_id(), link.id());
        assert_eq!(clicks[0].campaign_id(), Some(campaign_id));
        assert_eq!(clicks[0].referrer_host(), Some("social.example"));
        assert!(clicks[0].visitor_id().is_some());
        Ok(())
    }

    #[tokio::test]
    async fn fan_signup_is_private_and_propagates_server_request_id()
    -> Result<(), Box<dyn std::error::Error>> {
        let repository = Arc::new(TestRepository::happy()?);
        let repository_port: Arc<dyn AcquisitionRepository> = repository.clone();
        let workspace_id = WorkspaceId::new();
        let visitor_id = crowdrelay_domain::VisitorId::new();
        let app = test_router_with_state(state_with(
            repository_port,
            workspace_id,
            Arc::new(RedirectCache::new()),
            Arc::new(|_event| {}),
        )?)?;

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/fans")
                    .header(CONTENT_TYPE, "application/json")
                    .header("idempotency-key", "signup-test-0001")
                    .header(&X_REQUEST_ID, "must-be-replaced")
                    .header(COOKIE, format!("crowdrelay_attribution={visitor_id}"))
                    .body(Body::from(
                        r#"{"email":"Fan@Example.COM","display_name":"Ada","city_slug":"wroclaw","locale":"pl-PL","campaign_id":null,"consent":{"marketing":true,"policy_version":"privacy-v1"}}"#,
                    ))?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers()["cache-control"], "private, no-store");
        let fan_cookie = response.headers()[SET_COOKIE].to_str()?;
        assert!(fan_cookie.contains("crowdrelay_fan="));
        assert!(fan_cookie.contains("HttpOnly"));
        assert!(fan_cookie.contains("SameSite=Lax"));
        let response_request_id = response.headers()[&X_REQUEST_ID].to_str()?.to_owned();
        assert_ne!(response_request_id, "must-be-replaced");
        let body = to_bytes(response.into_body(), 16 * 1024).await?;
        let body: Value = serde_json::from_slice(&body)?;
        assert_eq!(body["status"], "active");
        assert_eq!(body["referral_url"], "http://localhost:4321/r/Fan_Code-123");

        let commands = repository
            .signup_commands
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].request_id().as_str(), response_request_id);
        assert_eq!(commands[0].signup().workspace_id(), workspace_id);
        assert_eq!(commands[0].signup().email().as_str(), "fan@example.com");
        assert_eq!(commands[0].signup().visitor_id(), Some(visitor_id));
        Ok(())
    }

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
    async fn privileged_namespaces_reject_cross_role_tokens()
    -> Result<(), Box<dyn std::error::Error>> {
        const ADMIN_KEY: &str = "test-admin-api-key-123456789012";
        const STAFF_KEY: &str = "test-staff-api-key-123456789012";
        const COMMERCE_KEY: &str = "test-commerce-api-key-1234567890";
        const AREA_KEY: &str = "test-area-management-key-1234567890";
        const CONTROL_PLANE_KEY: &str = "test-control-plane-key-123456789012";

        let app = test_router()?;
        for (uri, token) in [
            ("/v1/admin/events/test-show/ticketing", STAFF_KEY),
            ("/v1/admin/ops/summary", STAFF_KEY),
            ("/v1/staff/events/test-show/ticketing", COMMERCE_KEY),
            ("/v1/control-plane/area", ADMIN_KEY),
            ("/v1/control-plane/area", STAFF_KEY),
            ("/v1/control-plane/area", COMMERCE_KEY),
            ("/v1/admin/ops/summary", AREA_KEY),
            ("/v1/admin/ops/summary", CONTROL_PLANE_KEY),
            ("/v1/control-plane/ops/summary", ADMIN_KEY),
            ("/v1/control-plane/ops/summary", AREA_KEY),
            ("/v1/control-plane/ecosystem/flags", STAFF_KEY),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header(AUTHORIZATION, format!("Bearer {token}"))
                        .body(Body::empty())?,
                )
                .await?;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }

        let internal_with_admin = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/internal/ticket-orders/stripe-events")
                    .header(AUTHORIZATION, format!("Bearer {ADMIN_KEY}"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))?,
            )
            .await?;
        assert_eq!(internal_with_admin.status(), StatusCode::UNAUTHORIZED);

        for (uri, token) in [
            ("/v1/admin/events/test-show/ticketing", ADMIN_KEY),
            ("/v1/admin/ops/summary", ADMIN_KEY),
            ("/v1/staff/events/test-show/ticketing", STAFF_KEY),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header(AUTHORIZATION, format!("Bearer {token}"))
                        .body(Body::empty())?,
                )
                .await?;
            assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
        }

        let internal_with_commerce = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/internal/ticket-orders/stripe-events")
                    .header(AUTHORIZATION, format!("Bearer {COMMERCE_KEY}"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))?,
            )
            .await?;
        assert_ne!(internal_with_commerce.status(), StatusCode::UNAUTHORIZED);

        let control_plane_with_control_plane_key = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/control-plane/ops/summary")
                    .header(AUTHORIZATION, format!("Bearer {CONTROL_PLANE_KEY}"))
                    .body(Body::empty())?,
            )
            .await?;
        assert_ne!(control_plane_with_control_plane_key.status(), StatusCode::UNAUTHORIZED);

        let area_with_area_key = app
            .oneshot(
                Request::builder()
                    .uri("/v1/control-plane/area")
                    .header(AUTHORIZATION, format!("Bearer {AREA_KEY}"))
                    .body(Body::empty())?,
            )
            .await?;
        assert_ne!(area_with_area_key.status(), StatusCode::UNAUTHORIZED);
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
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(redirect.status(), StatusCode::FOUND);
        assert_eq!(redirect.headers()[LOCATION], "http://localhost:4321/join");
        let referral_cookie = redirect.headers()[SET_COOKIE].to_str()?;
        assert!(referral_cookie.contains("crowdrelay_referral=Fan_Code-123"));
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
    async fn fan_signup_requires_idempotency_and_explicit_consent()
    -> Result<(), Box<dyn std::error::Error>> {
        let repository = Arc::new(TestRepository::happy()?);
        let repository_port: Arc<dyn AcquisitionRepository> = repository.clone();
        let workspace_id = WorkspaceId::new();
        let app = test_router_with_state(state_with(
            repository_port,
            workspace_id,
            Arc::new(RedirectCache::new()),
            Arc::new(|_event| {}),
        )?)?;
        let body = r#"{"email":"fan@example.com","city_slug":"wroclaw","consent":{"marketing":false,"policy_version":"privacy-v1"}}"#;

        let missing_key = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/fans")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(body))?,
            )
            .await?;
        assert_eq!(missing_key.status(), StatusCode::BAD_REQUEST);

        let refused_consent = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/fans")
                    .header(CONTENT_TYPE, "application/json")
                    .header("idempotency-key", "signup-test-0002")
                    .body(Body::from(body))?,
            )
            .await?;
        assert_eq!(refused_consent.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            repository
                .signup_commands
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn fan_signup_rejects_oversized_bodies_with_problem_details()
    -> Result<(), Box<dyn std::error::Error>> {
        let payload = format!(
            r#"{{"email":"fan@example.com","display_name":"{}","city_slug":"wroclaw","consent":{{"marketing":true,"policy_version":"privacy-v1"}}}}"#,
            "x".repeat(super::MAX_PUBLIC_BODY_BYTES)
        );
        let response = test_router()?
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/fans")
                    .header(CONTENT_TYPE, "application/json")
                    .header("idempotency-key", "signup-test-large")
                    .body(Body::from(payload))?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(response.headers()[CONTENT_TYPE], "application/problem+json");
        assert_eq!(response.headers()["cache-control"], "private, no-store");
        Ok(())
    }

    #[tokio::test]
    async fn public_cities_support_strong_etag_revalidation()
    -> Result<(), Box<dyn std::error::Error>> {
        let repository: Arc<dyn AcquisitionRepository> = Arc::new(TestRepository::happy()?);
        let app = test_router_with_state(state_with(
            repository,
            WorkspaceId::new(),
            Arc::new(RedirectCache::new()),
            Arc::new(|_event| {}),
        )?)?;

        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/public/cities?limit=20")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(
            first.headers()["cache-control"],
            "public, max-age=60, stale-while-revalidate=600, stale-if-error=86400"
        );
        let etag = first.headers()[ETAG].clone();
        let body = to_bytes(first.into_body(), 16 * 1024).await?;
        let body: Value = serde_json::from_slice(&body)?;
        assert_eq!(body["items"][0]["slug"], "wroclaw");
        assert_eq!(body["items"][0]["fan_count"], 42);

        let revalidated = app
            .oneshot(
                Request::builder()
                    .uri("/v1/public/cities?limit=20")
                    .header(IF_NONE_MATCH, etag)
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(revalidated.status(), StatusCode::NOT_MODIFIED);
        assert!(revalidated.headers().contains_key(ETAG));
        assert!(
            to_bytes(revalidated.into_body(), 16 * 1024)
                .await?
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn rate_limited_public_auth_requests_receive_429_with_retry_after()
    -> Result<(), Box<dyn std::error::Error>> {
        let limiter = Arc::new(crate::RateLimiter::new(crate::RateLimitPolicy {
            enabled: true,
            public_auth_per_minute: 1,
            privileged_per_minute: 1000,
            general_per_minute: 1000,
        }));
        let http_config = HttpConfig::new(["http://localhost:4321".to_owned()])?
            .with_rate_limit(Some(limiter));
        let app = router(unavailable_state()?, http_config);

        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/fans/access")
                    .header("content-type", "application/json")
                    .header("x-forwarded-for", "203.0.113.50")
                    .body(Body::from(r#"{"email":"fan@example.com"}"#))?,
            )
            .await?;
        assert_ne!(first.status(), StatusCode::TOO_MANY_REQUESTS);

        let second = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/fans/access")
                    .header("content-type", "application/json")
                    .header("x-forwarded-for", "203.0.113.50")
                    .body(Body::from(r#"{"email":"fan@example.com"}"#))?,
            )
            .await?;
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(second.headers().contains_key("retry-after"));
        assert_eq!(
            second.headers()["cache-control"],
            "no-store"
        );
        let body = to_bytes(second.into_body(), 2048).await?;
        assert!(String::from_utf8_lossy(&body).contains("rate-limited"));
        Ok(())
    }

    #[tokio::test]
    async fn rate_limiting_is_isolated_per_identity_and_skips_health()
    -> Result<(), Box<dyn std::error::Error>> {
        let limiter = Arc::new(crate::RateLimiter::new(crate::RateLimitPolicy {
            enabled: true,
            public_auth_per_minute: 1,
            privileged_per_minute: 1000,
            general_per_minute: 1000,
        }));
        let http_config = HttpConfig::new(["http://localhost:4321".to_owned()])?
            .with_rate_limit(Some(limiter));
        let app = router(unavailable_state()?, http_config);

        async fn post_from(
            app: axum::Router,
            ip: &'static str,
        ) -> Result<axum::response::Response, Box<dyn std::error::Error>> {
            Ok(app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/fans/access")
                        .header("x-forwarded-for", ip)
                        .body(Body::empty())?,
                )
                .await?)
        }
        let first = post_from(app.clone(), "198.51.100.10").await?;
        assert_ne!(first.status(), StatusCode::TOO_MANY_REQUESTS);
        let other = post_from(app.clone(), "198.51.100.11").await?;
        assert_ne!(other.status(), StatusCode::TOO_MANY_REQUESTS);

        let repeat = post_from(app.clone(), "198.51.100.10").await?;
        assert_eq!(repeat.status(), StatusCode::TOO_MANY_REQUESTS);

        let health = app
            .oneshot(Request::builder().uri("/health/live").body(Body::empty())?)
            .await?;
        assert_eq!(health.status(), StatusCode::OK);
        Ok(())
    }
}
