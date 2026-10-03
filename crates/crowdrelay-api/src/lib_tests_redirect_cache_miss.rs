// A link minted by the worker may go live at the provider before the API's
// periodic redirect snapshot refresh sees it. Cache lag must not turn that
// valid first click into a 404. `include!`d into `mod tests` in
// `lib_tests.rs`, so it shares the route/state helpers.

    #[tokio::test]
    async fn redirect_cache_miss_resolves_a_fresh_repository_link()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace_id = WorkspaceId::new();
        let campaign_id = CampaignId::new();
        let link = ResolvedSmartLink::new(
            SmartLinkId::new(),
            workspace_id,
            Some(campaign_id),
            SmartLinkSlug::parse("fresh-social-link")?,
            DestinationUrl::parse("https://virya.music/signal/")?,
            1,
            Some("facebook".to_owned()),
            None,
        )?;

        // Intentionally empty: this is exactly the interval after the worker
        // commits the smart_links row and before the API snapshot refreshes.
        let cache = Arc::new(RedirectCache::new());
        let repository: Arc<dyn AcquisitionRepository> = Arc::new(
            TestRepository::happy()?.with_smart_links(vec![link.clone()]),
        );
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let click_capture = Arc::clone(&clicks);
        let app = test_router_with_state(state_with(
            repository,
            workspace_id,
            cache,
            Arc::new(move |event| {
                click_capture
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(event);
            Box::pin(async { ClickSubmission::Accepted })
            }),
        )?)?;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/go/fresh-social-link")
                    .header(REFERER, "https://facebook.com/virya/posts/123")
                    .header("user-agent", "Mozilla/5.0 Chrome/154 Safari/537.36")
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(response.headers()[LOCATION], "https://virya.music/signal/");
        assert!(
            response.headers().get(SET_COOKIE).is_some(),
            "the first real visitor still gets the attribution cookie"
        );

        let clicks = clicks.lock().unwrap_or_else(|error| error.into_inner());
        assert_eq!(clicks.len(), 1);
        assert_eq!(clicks[0].smart_link_id(), link.id());
        assert_eq!(clicks[0].campaign_id(), Some(campaign_id));
        assert_eq!(clicks[0].visitor_id().is_some(), true);
        Ok(())
    }

    #[tokio::test]
    async fn redirect_cache_miss_distinguishes_absent_from_unverifiable()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace_id = WorkspaceId::new();

        // Repository is healthy and says the slug does not exist: real 404.
        let healthy: Arc<dyn AcquisitionRepository> = Arc::new(TestRepository::happy()?);
        let healthy_app = test_router_with_state(state_with(
            healthy,
            workspace_id,
            Arc::new(RedirectCache::new()),
            Arc::new(|_event| Box::pin(async { ClickSubmission::Accepted })),
        )?)?;
        let missing = healthy_app
            .oneshot(
                Request::builder()
                    .uri("/v1/go/not-a-real-link")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        // Repository is unavailable while the snapshot also misses. The link
        // may simply be newer than the snapshot, so do not tell a real human
        // it does not exist. 503 is retryable and truthful.
        let unavailable: Arc<dyn AcquisitionRepository> =
            Arc::new(TestRepository::unavailable());
        let unavailable_app = test_router_with_state(state_with(
            unavailable,
            workspace_id,
            Arc::new(RedirectCache::new()),
            Arc::new(|_event| Box::pin(async { ClickSubmission::Accepted })),
        )?)?;
        let unknown = unavailable_app
            .oneshot(
                Request::builder()
                    .uri("/v1/go/maybe-fresh-link")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(unknown.status(), StatusCode::SERVICE_UNAVAILABLE);
        Ok(())
    }
