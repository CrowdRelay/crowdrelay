// A link preview, a crawler, a HEAD probe and a browser prefetch are served the
// redirect but are not an audience. `include!`d into `mod tests` in
// `lib_tests.rs` so it shares that scope's helpers and imports.

    #[tokio::test]
    async fn automated_fetchers_get_the_redirect_but_no_click_and_no_cookie()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace_id = WorkspaceId::new();
        let link = ResolvedSmartLink::new(
            SmartLinkId::new(),
            workspace_id,
            None,
            SmartLinkSlug::parse("tour-2026")?,
            DestinationUrl::parse("https://virya.music/join")?,
            1,
            None,
            None,
        )?;
        let cache = Arc::new(RedirectCache::new());
        cache.replace([link], Vec::new())?;
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

        let browser = "Mozilla/5.0 (Windows NT 10.0) AppleWebKit/537.36 Chrome/129.0 Safari/537.36";
        let requests = [
            ("GET", "Mozilla/5.0 (compatible; Discordbot/2.0)", None),
            ("GET", "TelegramBot (like TwitterBot)", None),
            ("HEAD", browser, None),
            ("GET", browser, Some(("sec-purpose", "prefetch"))),
        ];
        for (method, agent, extra) in requests {
            let mut request = Request::builder()
                .method(method)
                .uri("/v1/go/tour-2026")
                .header("user-agent", agent);
            if let Some((name, value)) = extra {
                request = request.header(name, value);
            }
            let response = app.clone().oneshot(request.body(Body::empty())?).await?;
            assert_eq!(response.status(), StatusCode::FOUND, "{method} {agent}");
            assert_eq!(response.headers()[LOCATION], "https://virya.music/join");
            assert!(
                response.headers().get(SET_COOKIE).is_none(),
                "{method} {agent} must not be given an attribution cookie"
            );
        }
        assert!(
            clicks.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "none of them is a click"
        );

        // The control: a person on the same link IS recorded.
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/go/tour-2026")
                    .header("user-agent", browser)
                    .body(Body::empty())?,
            )
            .await?;
        assert!(response.headers().get(SET_COOKIE).is_some());
        assert_eq!(clicks.lock().unwrap_or_else(|e| e.into_inner()).len(), 1);
        Ok(())
    }
