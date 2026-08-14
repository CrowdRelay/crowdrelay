#[cfg(test)]
mod tests {
    use super::*;

    const DATABASE_URL: &str = "postgres://user:highly-secret@localhost/crowdrelay";
    const ALLOWED_ORIGINS: &str = "http://localhost:4321";
    const WORKSPACE_SLUG: &str = "virya";
    const PUBLIC_SITE_BASE_URL: &str = "http://localhost:4321";

    fn config_with(overrides: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let mut values = vec![
            (DATABASE_URL_KEY, DATABASE_URL),
            (ALLOWED_ORIGINS_KEY, ALLOWED_ORIGINS),
            (WORKSPACE_SLUG_KEY, WORKSPACE_SLUG),
            (PUBLIC_SITE_BASE_URL_KEY, PUBLIC_SITE_BASE_URL),
        ];
        values.extend_from_slice(overrides);
        Config::from_values(values)
    }

    #[test]
    fn defaults_are_safe_and_bounded() -> Result<(), Box<dyn std::error::Error>> {
        let config = config_with(&[])?;

        assert_eq!(config.environment, Environment::Development);
        assert_eq!(config.bind_addr, DEFAULT_BIND_ADDR.parse::<SocketAddr>()?);
        assert_eq!(
            config.database.max_connections,
            DEFAULT_DATABASE_MAX_CONNECTIONS
        );
        assert_eq!(
            config.database.connect_timeout,
            Duration::from_millis(DEFAULT_DATABASE_CONNECT_TIMEOUT_MS)
        );
        assert_eq!(
            config.database.ping_timeout,
            Duration::from_millis(DEFAULT_DATABASE_PING_TIMEOUT_MS)
        );
        assert_eq!(
            config.database.operation_timeout,
            Duration::from_millis(DEFAULT_DATABASE_OPERATION_TIMEOUT_MS)
        );
        assert_eq!(
            config.database.lock_timeout,
            Duration::from_millis(DEFAULT_DATABASE_LOCK_TIMEOUT_MS)
        );
        assert_eq!(config.allowed_origins, [ALLOWED_ORIGINS]);
        assert!(!config.random_draws_enabled);
        assert!(config.commerce_api_key_sha256.is_none());
        assert!(config.require_double_opt_in);
        assert!(config.admission_security.admin_api_key_sha256.is_none());
        assert!(config.admission_security.staff_api_key_sha256.is_none());
        assert!(config.admission_security.qr_signing_key.is_none());
        assert_eq!(
            config.response_encryption_key,
            SensitiveResponseKey::derive_from_secret(LOCAL_RESPONSE_ENCRYPTION_SECRET.as_bytes())
        );
        assert!(config.previous_response_encryption_key.is_none());
        assert_eq!(config.workspace_slug.as_str(), WORKSPACE_SLUG);
        assert_eq!(
            config.public_site_base_url.as_str(),
            "http://localhost:4321/"
        );
        assert_eq!(config.default_country_code.as_str(), DEFAULT_COUNTRY_CODE);
        assert_eq!(
            config.redirect_refresh_interval,
            Duration::from_millis(DEFAULT_REDIRECT_REFRESH_INTERVAL_MS)
        );
        assert_eq!(
            config.click_buffer,
            ClickBufferConfig {
                capacity: DEFAULT_CLICK_CHANNEL_CAPACITY as usize,
                batch_size: DEFAULT_CLICK_BATCH_SIZE as usize,
                flush_interval: Duration::from_millis(DEFAULT_CLICK_FLUSH_INTERVAL_MS),
            }
        );
        Ok(())
    }

    #[test]
    fn parses_explicit_overrides() -> Result<(), Box<dyn std::error::Error>> {
        let config = config_with(&[
            (ENVIRONMENT_KEY, "production"),
            (BIND_ADDR_KEY, "127.0.0.1:9000"),
            (DATABASE_MAX_CONNECTIONS_KEY, "24"),
            (DATABASE_CONNECT_TIMEOUT_MS_KEY, "1500"),
            (DATABASE_PING_TIMEOUT_MS_KEY, "750"),
            (DATABASE_OPERATION_TIMEOUT_MS_KEY, "4000"),
            (DATABASE_LOCK_TIMEOUT_MS_KEY, "600"),
            (
                ALLOWED_ORIGINS_KEY,
                " https://virya.music, https://example.com:8443 ",
            ),
            (RANDOM_DRAWS_ENABLED_KEY, "false"),
            (WORKSPACE_SLUG_KEY, "virya-signal"),
            (PUBLIC_SITE_BASE_URL_KEY, "https://virya.music"),
            (DEFAULT_COUNTRY_CODE_KEY, "DE"),
            (REDIRECT_REFRESH_INTERVAL_MS_KEY, "45000"),
            (CLICK_CHANNEL_CAPACITY_KEY, "8192"),
            (CLICK_BATCH_SIZE_KEY, "500"),
            (CLICK_FLUSH_INTERVAL_MS_KEY, "250"),
            (COMMERCE_API_KEY, "test-commerce-api-key-1234567890"),
            (ADMIN_API_KEY_KEY, "test-admin-api-key-12345678901234567890"),
            (STAFF_API_KEY_KEY, "test-staff-api-key-12345678901234567890"),
            (
                QR_SIGNING_SECRET_KEY,
                "test-qr-signing-secret-123456789012345",
            ),
            (
                RESPONSE_ENCRYPTION_SECRET_KEY,
                "test-response-encryption-secret-1234567890",
            ),
            (
                PREVIOUS_RESPONSE_ENCRYPTION_SECRET_KEY,
                "previous-response-encryption-secret-123456",
            ),
            (QR_TTL_SECONDS_KEY, "45"),
            (REQUIRE_DOUBLE_OPT_IN_KEY, "false"),
        ])?;

        assert_eq!(config.environment.to_string(), "production");
        assert_eq!(config.bind_addr, "127.0.0.1:9000".parse()?);
        assert_eq!(config.database.max_connections, 24);
        assert_eq!(config.database.connect_timeout, Duration::from_millis(1500));
        assert_eq!(config.database.ping_timeout, Duration::from_millis(750));
        assert_eq!(
            config.database.operation_timeout,
            Duration::from_millis(4000)
        );
        assert_eq!(config.database.lock_timeout, Duration::from_millis(600));
        assert_eq!(
            config.allowed_origins,
            ["https://virya.music", "https://example.com:8443"]
        );
        assert!(!config.random_draws_enabled);
        assert_eq!(config.workspace_slug.as_str(), "virya-signal");
        assert_eq!(config.public_site_base_url.as_str(), "https://virya.music/");
        assert_eq!(config.default_country_code.as_str(), "DE");
        assert_eq!(
            config.redirect_refresh_interval,
            Duration::from_millis(45_000)
        );
        assert_eq!(config.click_buffer.capacity, 8_192);
        assert_eq!(config.click_buffer.batch_size, 500);
        assert_eq!(
            config.click_buffer.flush_interval,
            Duration::from_millis(250)
        );
        assert_eq!(
            config.commerce_api_key_sha256,
            Some(Sha256::digest(b"test-commerce-api-key-1234567890").into())
        );
        assert_eq!(config.admission_security.qr_ttl, Duration::from_secs(45));
        assert!(config.admission_security.admin_api_key_sha256.is_some());
        assert!(config.admission_security.staff_api_key_sha256.is_some());
        assert!(config.admission_security.qr_signing_key.is_some());
        assert_eq!(
            config.response_encryption_key,
            SensitiveResponseKey::derive_from_secret(b"test-response-encryption-secret-1234567890")
        );
        assert_eq!(
            config.previous_response_encryption_key,
            Some(SensitiveResponseKey::derive_from_secret(
                b"previous-response-encryption-secret-123456"
            ))
        );
        assert!(!config.require_double_opt_in);
        Ok(())
    }

    #[test]
    fn production_requires_response_encryption_secret() {
        let error = config_with(&[
            (ENVIRONMENT_KEY, "production"),
            (ALLOWED_ORIGINS_KEY, "https://virya.music"),
            (PUBLIC_SITE_BASE_URL_KEY, "https://virya.music"),
            (ADMIN_API_KEY_KEY, "test-admin-api-key-12345678901234567890"),
            (STAFF_API_KEY_KEY, "test-staff-api-key-12345678901234567890"),
            (
                QR_SIGNING_SECRET_KEY,
                "test-qr-signing-secret-123456789012345",
            ),
        ])
        .expect_err("production must require response encryption");

        assert!(matches!(
            error,
            ConfigError::Missing {
                name: RESPONSE_ENCRYPTION_SECRET_KEY
            }
        ));
    }

    #[test]
    fn production_rejects_published_response_encryption_sentinels() {
        for secret in [
            LOCAL_RESPONSE_ENCRYPTION_SECRET,
            "REPLACE_RESPONSE_ENCRYPTION_SECRET",
        ] {
            let error = config_with(&[
                (ENVIRONMENT_KEY, "production"),
                (ALLOWED_ORIGINS_KEY, "https://virya.music"),
                (PUBLIC_SITE_BASE_URL_KEY, "https://virya.music"),
                (ADMIN_API_KEY_KEY, "test-admin-api-key-12345678901234567890"),
                (STAFF_API_KEY_KEY, "test-staff-api-key-12345678901234567890"),
                (
                    QR_SIGNING_SECRET_KEY,
                    "test-qr-signing-secret-123456789012345",
                ),
                (RESPONSE_ENCRYPTION_SECRET_KEY, secret),
            ])
            .expect_err("published encryption secrets must fail closed in production");
            assert!(matches!(
                error,
                ConfigError::InvalidSecret {
                    name: RESPONSE_ENCRYPTION_SECRET_KEY
                }
            ));
        }
    }

    #[test]
    fn production_rejects_published_previous_encryption_sentinels() {
        for previous_secret in [
            LOCAL_RESPONSE_ENCRYPTION_SECRET,
            "REPLACE_RESPONSE_ENCRYPTION_SECRET",
        ] {
            let error = config_with(&[
                (ENVIRONMENT_KEY, "production"),
                (ALLOWED_ORIGINS_KEY, "https://virya.music"),
                (PUBLIC_SITE_BASE_URL_KEY, "https://virya.music"),
                (ADMIN_API_KEY_KEY, "test-admin-api-key-12345678901234567890"),
                (STAFF_API_KEY_KEY, "test-staff-api-key-12345678901234567890"),
                (
                    QR_SIGNING_SECRET_KEY,
                    "test-qr-signing-secret-123456789012345",
                ),
                (
                    RESPONSE_ENCRYPTION_SECRET_KEY,
                    "test-current-response-encryption-secret-1234567890",
                ),
                (PREVIOUS_RESPONSE_ENCRYPTION_SECRET_KEY, previous_secret),
            ])
            .expect_err("published previous keys must fail closed in production");
            assert!(matches!(
                error,
                ConfigError::InvalidSecret {
                    name: PREVIOUS_RESPONSE_ENCRYPTION_SECRET_KEY
                }
            ));
        }
    }

    #[test]
    fn rejects_identical_current_and_previous_encryption_keys() {
        let shared_secret = "test-shared-response-encryption-secret-1234567890";
        let error = config_with(&[
            (RESPONSE_ENCRYPTION_SECRET_KEY, shared_secret),
            (PREVIOUS_RESPONSE_ENCRYPTION_SECRET_KEY, shared_secret),
        ])
        .expect_err("a previous key equal to the current key is a rollout error");

        assert!(matches!(
            error,
            ConfigError::InvalidSecret {
                name: PREVIOUS_RESPONSE_ENCRYPTION_SECRET_KEY
            }
        ));
    }

    #[test]
    fn validates_response_encryption_secret_and_redacts_it()
    -> Result<(), Box<dyn std::error::Error>> {
        let secret = "test-response-encryption-secret-1234567890";
        let config = config_with(&[(RESPONSE_ENCRYPTION_SECRET_KEY, secret)])?;

        assert_eq!(
            config.response_encryption_key,
            SensitiveResponseKey::derive_from_secret(secret.as_bytes())
        );
        assert!(!format!("{config:?}").contains(secret));
        for invalid in [
            "too-short",
            "contains whitespace but is definitely long enough",
            "contains-a-newline-but-is-long-enough\n1234567890",
        ] {
            assert!(matches!(
                config_with(&[(RESPONSE_ENCRYPTION_SECRET_KEY, invalid)]),
                Err(ConfigError::InvalidSecret { .. })
            ));
        }
        Ok(())
    }

    #[test]
    fn requires_database_url() {
        let error = Config::from_values([
            (ALLOWED_ORIGINS_KEY, ALLOWED_ORIGINS),
            (WORKSPACE_SLUG_KEY, WORKSPACE_SLUG),
            (PUBLIC_SITE_BASE_URL_KEY, PUBLIC_SITE_BASE_URL),
        ])
        .expect_err("database URL must be required");

        assert!(matches!(
            error,
            ConfigError::Missing {
                name: DATABASE_URL_KEY
            }
        ));
    }

    #[test]
    fn rejects_invalid_database_url_without_echoing_secret() {
        let secret = "secret-that-must-not-leak";
        let error = Config::from_values([
            (
                DATABASE_URL_KEY.to_owned(),
                format!("not-a-postgres-url:{secret}"),
            ),
            (ALLOWED_ORIGINS_KEY.to_owned(), ALLOWED_ORIGINS.to_owned()),
            (WORKSPACE_SLUG_KEY.to_owned(), WORKSPACE_SLUG.to_owned()),
            (
                PUBLIC_SITE_BASE_URL_KEY.to_owned(),
                PUBLIC_SITE_BASE_URL.to_owned(),
            ),
        ])
        .expect_err("invalid database URL must fail");

        assert!(matches!(error, ConfigError::InvalidDatabaseUrl { .. }));
        assert!(!error.to_string().contains(secret));
    }

    #[test]
    fn enforces_pool_size_bounds() {
        for value in ["0", "101"] {
            let error = config_with(&[(DATABASE_MAX_CONNECTIONS_KEY, value)])
                .expect_err("out-of-range pool size must fail");
            assert!(matches!(error, ConfigError::OutOfRange { .. }));
        }
    }

    #[test]
    fn enforces_timeout_bounds() {
        for value in ["0", "60001"] {
            let error = config_with(&[(DATABASE_PING_TIMEOUT_MS_KEY, value)])
                .expect_err("out-of-range timeout must fail");
            assert!(matches!(error, ConfigError::OutOfRange { .. }));
        }

        let error = config_with(&[
            (DATABASE_OPERATION_TIMEOUT_MS_KEY, "500"),
            (DATABASE_LOCK_TIMEOUT_MS_KEY, "501"),
        ])
        .expect_err("lock timeout cannot exceed the whole operation timeout");
        assert!(matches!(
            error,
            ConfigError::LockTimeoutExceedsOperationTimeout
        ));
    }

    #[test]
    fn validates_commerce_api_key_without_storing_plaintext()
    -> Result<(), Box<dyn std::error::Error>> {
        let secret = "test-commerce-api-key-1234567890";
        let config = config_with(&[(COMMERCE_API_KEY, secret)])?;
        assert_eq!(
            config.commerce_api_key_sha256,
            Some(Sha256::digest(secret.as_bytes()).into())
        );

        for invalid in [
            "short",
            " contains-leading-space-1234567890",
            "contains space but long enough 1234567890",
        ] {
            assert!(matches!(
                config_with(&[(COMMERCE_API_KEY, invalid)]),
                Err(ConfigError::InvalidSecret { .. })
            ));
        }
        Ok(())
    }

    #[test]
    fn random_draws_fail_closed() -> Result<(), Box<dyn std::error::Error>> {
        assert!(!config_with(&[])?.random_draws_enabled);
        assert!(!config_with(&[(RANDOM_DRAWS_ENABLED_KEY, "false")])?.random_draws_enabled);
        assert!(config_with(&[(RANDOM_DRAWS_ENABLED_KEY, "true")])?.random_draws_enabled);

        for value in ["TRUE", "yes", "1", " false "] {
            let result = config_with(&[(RANDOM_DRAWS_ENABLED_KEY, value)]);
            if value == " false " {
                assert!(!result?.random_draws_enabled);
            } else {
                assert!(matches!(result, Err(ConfigError::InvalidBoolean { .. })));
            }
        }
        Ok(())
    }

    #[test]
    fn production_draws_require_explicit_opt_in() -> Result<(), Box<dyn std::error::Error>> {
        let config = config_with(&[
            (ENVIRONMENT_KEY, "production"),
            (ALLOWED_ORIGINS_KEY, "https://virya.music"),
            (PUBLIC_SITE_BASE_URL_KEY, "https://virya.music"),
            (COMMERCE_API_KEY, "test-commerce-api-key-1234567890"),
            (ADMIN_API_KEY_KEY, "test-admin-api-key-12345678901234567890"),
            (STAFF_API_KEY_KEY, "test-staff-api-key-12345678901234567890"),
            (
                QR_SIGNING_SECRET_KEY,
                "test-qr-signing-secret-123456789012345",
            ),
            (
                RESPONSE_ENCRYPTION_SECRET_KEY,
                "test-response-encryption-secret-1234567890",
            ),
            (RANDOM_DRAWS_ENABLED_KEY, "true"),
        ])?;

        assert!(config.random_draws_enabled);
        Ok(())
    }

    #[test]
    fn validates_and_deduplicates_allowed_origins() -> Result<(), Box<dyn std::error::Error>> {
        let config = config_with(&[(
            ALLOWED_ORIGINS_KEY,
            " http://localhost:4321,https://example.com/,http://localhost:4321 ",
        )])?;

        assert_eq!(
            config.allowed_origins,
            ["http://localhost:4321", "https://example.com"]
        );

        for value in [
            "",
            "*",
            "https://example.com/path",
            "https://user@example.com",
            "https://example.com?query=true",
            "https://example.com,",
        ] {
            assert!(
                config_with(&[(ALLOWED_ORIGINS_KEY, value)]).is_err(),
                "{value:?} must not be accepted as an origin list"
            );
        }
        Ok(())
    }

    #[test]
    fn production_origins_require_https() {
        let error = config_with(&[
            (ENVIRONMENT_KEY, "production"),
            (ALLOWED_ORIGINS_KEY, "http://virya.music"),
        ])
        .expect_err("plain HTTP production origin must fail");

        assert!(matches!(
            error,
            ConfigError::InsecureProductionOrigin { .. }
        ));
    }

    #[test]
    fn validates_phase_one_identity_and_public_url() {
        for slug in ["", "Virya", "-virya", "virya signal", "żółw"] {
            let result = config_with(&[(WORKSPACE_SLUG_KEY, slug)]);
            assert!(result.is_err(), "{slug:?} must not be accepted as a slug");
        }

        for url in [
            "",
            "javascript:alert(1)",
            "https://virya.music/path",
            "https://user@virya.music",
            "https://virya.music?query=true",
        ] {
            let result = config_with(&[(PUBLIC_SITE_BASE_URL_KEY, url)]);
            assert!(
                result.is_err(),
                "{url:?} must not be accepted as a base URL"
            );
        }

        let error = config_with(&[
            (ENVIRONMENT_KEY, "production"),
            (ALLOWED_ORIGINS_KEY, "https://virya.music"),
            (PUBLIC_SITE_BASE_URL_KEY, "http://virya.music"),
        ])
        .expect_err("production public site must use HTTPS");
        assert!(matches!(
            error,
            ConfigError::InsecureProductionSiteUrl { .. }
        ));
    }

    #[test]
    fn validates_country_and_click_buffer_bounds() {
        for country in ["pL", "POL", "1A", ""] {
            assert!(
                config_with(&[(DEFAULT_COUNTRY_CODE_KEY, country)]).is_err(),
                "{country:?} must not be accepted as a country code"
            );
        }

        for (name, value) in [
            (CLICK_CHANNEL_CAPACITY_KEY, "0"),
            (CLICK_CHANNEL_CAPACITY_KEY, "65537"),
            (CLICK_BATCH_SIZE_KEY, "0"),
            (CLICK_BATCH_SIZE_KEY, "1001"),
            (CLICK_FLUSH_INTERVAL_MS_KEY, "9"),
            (CLICK_FLUSH_INTERVAL_MS_KEY, "60001"),
            (REDIRECT_REFRESH_INTERVAL_MS_KEY, "999"),
            (REDIRECT_REFRESH_INTERVAL_MS_KEY, "600001"),
        ] {
            assert!(
                config_with(&[(name, value)]).is_err(),
                "{name}={value} must be rejected"
            );
        }

        let error = config_with(&[
            (CLICK_CHANNEL_CAPACITY_KEY, "10"),
            (CLICK_BATCH_SIZE_KEY, "11"),
        ])
        .expect_err("batch larger than channel capacity must fail");
        assert!(matches!(error, ConfigError::BatchExceedsCapacity { .. }));
    }

    #[test]
    fn database_debug_output_redacts_credentials() -> Result<(), Box<dyn std::error::Error>> {
        let config = config_with(&[])?;
        let output = format!("{config:?}");

        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("highly-secret"));
        assert!(!output.contains(DATABASE_URL));
        Ok(())
    }
}
