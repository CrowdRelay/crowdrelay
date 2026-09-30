// The test doubles every route test in `lib_tests.rs` builds its state from —
// one stub per repository port, each returning the shape the handler expects
// rather than touching a database.
//
// Split out of `lib_tests.rs` because the source ratchet's 1200-line budget
// applies to test code the same as product code, and the stub set grows by a
// method every time a port does. The same `include!` pattern the autopilot
// repository and `gig_plan` already use.
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

        async fn load_redirect_context(
            &self,
        ) -> Result<Option<crowdrelay_application::RedirectContext>, RepositoryError> {
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
        async fn create_event(
            &self,
            _command: &CreateEventCommand,
        ) -> Result<CreatedEvent, RepositoryError> {
            Err(RepositoryError::Unavailable)
        }

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

        async fn replace_event_acts(
            &self,
            _command: &crowdrelay_application::ReplaceEventActsCommand,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }

        async fn set_event_counterparty(
            &self,
            _command: &crowdrelay_application::SetEventCounterpartyCommand,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }

        async fn set_event_support_slots(
            &self,
            _command: &crowdrelay_application::SetEventSupportSlotsCommand,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }

        async fn set_event_festival(
            &self,
            _command: &crowdrelay_application::SetEventFestivalCommand,
        ) -> Result<(), RepositoryError> {
            Ok(())
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
            ListFanEventInterests::new(Arc::clone(&repository)),
            crowdrelay_application::CreateEvent::new(Arc::clone(&repository)),
            ReplaceEventActs::new(Arc::clone(&repository)),
            SetEventCounterparty::new(Arc::clone(&repository)),
            crowdrelay_application::SetEventSupportSlots::new(Arc::clone(&repository)),
            crowdrelay_application::SetEventFestival::new(repository),
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
        watch_origin: Option<Url>,
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
            watch_origin,
        }))
    }

