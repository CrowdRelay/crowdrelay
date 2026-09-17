// Attestation route tests — share-token reads, digest verification, and the
// control-plane issue/revoke/rotate trio. `include!`d into `mod tests` in
// `lib_tests.rs` so it shares that scope's helpers and imports.

    #[tokio::test]
    async fn attestation_public_routes_resolve()
    -> Result<(), Box<dyn std::error::Error>> {
        let app = test_router()?;
        // A malformed digest is refused by the route itself; a well-formed
        // one reaches the repository — 503 on the dead pool proves the wiring.
        for (uri, status) in [
            (
                "/v1/public/attestations/verify/not-a-digest".to_owned(),
                StatusCode::NOT_FOUND,
            ),
            (
                format!("/v1/public/attestations/verify/{}", "a".repeat(64)),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                format!("/v1/public/attestations/{}", uuid::Uuid::new_v4()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty())?)
                .await?;
            assert_eq!(response.status(), status);
        }
        Ok(())
    }

    #[tokio::test]
    async fn attestation_control_plane_routes_require_the_key()
    -> Result<(), Box<dyn std::error::Error>> {
        const ADMIN_KEY: &str = "test-admin-api-key-123456789012";
        const CONTROL_PLANE_KEY: &str = "test-control-plane-key-123456789012";
        let app = test_router()?;
        let digest = "b".repeat(64);
        for (method, uri) in [
            ("GET", "/v1/control-plane/attestations".to_owned()),
            ("POST", "/v1/control-plane/attestations".to_owned()),
            ("POST", format!("/v1/control-plane/attestations/{digest}/revoke")),
            ("POST", format!("/v1/control-plane/attestations/{digest}/rotate")),
        ] {
            for (token, expect_rejected) in [(ADMIN_KEY, true), (CONTROL_PLANE_KEY, false)] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method(method)
                            .uri(&uri)
                            .header(AUTHORIZATION, format!("Bearer {token}"))
                            .header(CONTENT_TYPE, "application/json")
                            .body(Body::from("{}"))?,
                    )
                    .await?;
                assert_eq!(
                    response.status() == StatusCode::UNAUTHORIZED,
                    expect_rejected,
                    "{method} {uri}"
                );
            }
        }

        // The cities bound is refused before the repository is touched — a
        // dead pool proves the request never reached it.
        let oversized = format!(
            "{{\"cities\":[{}]}}",
            (0..33)
                .map(|i| format!("\"city-{i}\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/control-plane/attestations")
                    .header(AUTHORIZATION, format!("Bearer {CONTROL_PLANE_KEY}"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(oversized))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

