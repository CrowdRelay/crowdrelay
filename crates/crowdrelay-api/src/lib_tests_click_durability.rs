// Lossless human-click boundary tests. Included into `mod tests` in
// `lib_tests.rs`, so these share the ordinary route/state harness.

    #[tokio::test]
    async fn human_redirect_fails_closed_when_click_has_no_durable_path()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace_id = WorkspaceId::new();
        let link = ResolvedSmartLink::new(
            SmartLinkId::new(),
            workspace_id,
            None,
            SmartLinkSlug::parse("proof-required")?,
            DestinationUrl::parse("https://virya.music/join")?,
            1,
            Some("community".to_owned()),
            None,
        )?;
        let cache = Arc::new(RedirectCache::new());
        cache.replace([link], Vec::new())?;

        let repository: Arc<dyn AcquisitionRepository> =
            Arc::new(TestRepository::unavailable());
        let app = test_router_with_state(state_with(
            repository,
            workspace_id,
            cache,
            Arc::new(|_event| Box::pin(async { ClickSubmission::Unavailable })),
        )?)?;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/go/proof-required")
                    .header(
                        USER_AGENT,
                        "Mozilla/5.0 AppleWebKit/537.36 Chrome/154 Safari/537.36",
                    )
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "without a queued or durable click receipt the acquisition flow must stop"
        );
        assert!(
            response.headers().get(LOCATION).is_none(),
            "do not send the person to signup after losing the only causal proof"
        );
        assert!(
            response.headers().get(SET_COOKIE).is_none(),
            "never mint an attribution cookie for a click the system failed to retain"
        );
        Ok(())
    }
