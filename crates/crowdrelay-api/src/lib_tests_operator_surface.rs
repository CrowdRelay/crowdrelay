// The operator surface split out of `/v1/admin` — `control_plane_operator.rs`.
// `include!`d into `mod tests` in `lib_tests.rs` so it shares that scope's
// helpers and imports.

    /// Every route in `control_plane_operator.rs`, read from the file itself so
    /// a route added there is covered here without anyone listing it.
    fn operator_surface_routes() -> Vec<(String, String)> {
        let source = include_str!("control_plane_operator.rs");
        let mut routes = Vec::new();
        let mut rest = source;
        while let Some(start) = rest.find(".route(") {
            rest = &rest[start + ".route(".len()..];
            let Some(open) = rest.find('"') else { break };
            let Some(close) = rest[open + 1..].find('"') else { break };
            let path = rest[open + 1..open + 1 + close].to_owned();
            let expression_end = rest.find("\n        .").unwrap_or(rest.len());
            let expression = &rest[..expression_end];
            for method in ["get", "post", "put", "delete"] {
                if expression.contains(&format!("{method}(crate::"))
                    || expression.contains(&format!(".{method}(crate::"))
                {
                    routes.push((method.to_uppercase(), path.clone()));
                }
            }
        }
        routes
    }

    fn concrete(path: &str) -> String {
        path.split('/')
            .map(|segment| {
                if segment.starts_with('{') && segment.ends_with("_id}") {
                    "0190a0b0-0000-7000-8000-000000000001".to_owned()
                } else if segment.starts_with('{') {
                    "some-slug".to_owned()
                } else {
                    segment.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    #[test]
    fn operator_surface_parser_sees_the_routes() {
        // A parser that matched nothing would make the auth test vacuous.
        let routes = operator_surface_routes();
        assert!(routes.len() > 70, "parsed only {} routes", routes.len());
        assert!(routes.iter().all(|(_, path)| path.starts_with("/v1/control-plane/")));
    }

    /// Admin handlers reached through the control-plane namespace must take
    /// the ControlPlane bearer and nothing else — the admin key included, or
    /// this file would be the alias for `/v1/admin` that `control_plane.rs`
    /// promises never to become.
    #[tokio::test]
    async fn operator_surface_takes_only_the_control_plane_bearer()
    -> Result<(), Box<dyn std::error::Error>> {
        const ADMIN_KEY: &str = "test-admin-api-key-123456789012";
        const CONTROL_PLANE_KEY: &str = "test-control-plane-key-123456789012";
        let app = test_router()?;
        for (method, path) in operator_surface_routes() {
            let uri = concrete(&path);
            for token in [None, Some(ADMIN_KEY)] {
                let mut builder = Request::builder().method(method.as_str()).uri(&uri);
                if let Some(token) = token {
                    builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
                }
                let response = app.clone().oneshot(builder.body(Body::empty())?).await?;
                assert_eq!(
                    response.status(),
                    StatusCode::UNAUTHORIZED,
                    "{method} {uri} with {token:?} must be refused"
                );
            }
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method.as_str())
                        .uri(&uri)
                        .header(AUTHORIZATION, format!("Bearer {CONTROL_PLANE_KEY}"))
                        .body(Body::empty())?,
                )
                .await?;
            // Registered and authorised: anything but 401 (refused) and
            // 404/405 (not routed). The dead test pool answers 503 or the
            // handler refuses the empty body — both prove the wiring.
            assert!(
                !matches!(
                    response.status(),
                    StatusCode::UNAUTHORIZED
                        | StatusCode::NOT_FOUND
                        | StatusCode::METHOD_NOT_ALLOWED
                ),
                "{method} {uri} answered {} with the control-plane key",
                response.status()
            );
        }
        Ok(())
    }
