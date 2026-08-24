fn parse_environment(value: Option<&String>) -> Result<Environment, ConfigError> {
    match value.map(|value| value.trim()) {
        None | Some("") | Some("development") => Ok(Environment::Development),
        Some("test") => Ok(Environment::Test),
        Some("production") => Ok(Environment::Production),
        Some(_) => Err(ConfigError::InvalidEnvironment {
            name: ENVIRONMENT_KEY,
        }),
    }
}

fn parse_socket_addr(value: Option<&String>) -> Result<SocketAddr, ConfigError> {
    value
        .map_or(DEFAULT_BIND_ADDR, String::as_str)
        .trim()
        .parse()
        .map_err(|source| ConfigError::InvalidSocketAddress {
            name: BIND_ADDR_KEY,
            source,
        })
}

fn parse_database_config(values: &HashMap<String, String>) -> Result<DatabaseConfig, ConfigError> {
    let url = values
        .get(DATABASE_URL_KEY)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .ok_or(ConfigError::Missing {
            name: DATABASE_URL_KEY,
        })?;

    let parsed_url = Url::parse(url).map_err(|_| ConfigError::InvalidDatabaseUrl {
        name: DATABASE_URL_KEY,
    })?;
    if !matches!(parsed_url.scheme(), "postgres" | "postgresql") {
        return Err(ConfigError::InvalidDatabaseUrl {
            name: DATABASE_URL_KEY,
        });
    }

    PgConnectOptions::from_str(url).map_err(|_| ConfigError::InvalidDatabaseUrl {
        name: DATABASE_URL_KEY,
    })?;

    let max_connections = parse_bounded_u32(
        values.get(DATABASE_MAX_CONNECTIONS_KEY),
        DATABASE_MAX_CONNECTIONS_KEY,
        DEFAULT_DATABASE_MAX_CONNECTIONS,
        1,
        MAX_DATABASE_CONNECTIONS,
    )?;
    let connect_timeout = parse_duration(
        values.get(DATABASE_CONNECT_TIMEOUT_MS_KEY),
        DATABASE_CONNECT_TIMEOUT_MS_KEY,
        DEFAULT_DATABASE_CONNECT_TIMEOUT_MS,
    )?;
    let ping_timeout = parse_duration(
        values.get(DATABASE_PING_TIMEOUT_MS_KEY),
        DATABASE_PING_TIMEOUT_MS_KEY,
        DEFAULT_DATABASE_PING_TIMEOUT_MS,
    )?;
    let operation_timeout = parse_duration(
        values.get(DATABASE_OPERATION_TIMEOUT_MS_KEY),
        DATABASE_OPERATION_TIMEOUT_MS_KEY,
        DEFAULT_DATABASE_OPERATION_TIMEOUT_MS,
    )?;
    let lock_timeout = parse_bounded_duration(
        values.get(DATABASE_LOCK_TIMEOUT_MS_KEY),
        DATABASE_LOCK_TIMEOUT_MS_KEY,
        DEFAULT_DATABASE_LOCK_TIMEOUT_MS,
        1,
        MAX_DATABASE_LOCK_TIMEOUT_MS,
    )?;
    if lock_timeout > operation_timeout {
        return Err(ConfigError::LockTimeoutExceedsOperationTimeout);
    }

    Ok(DatabaseConfig {
        url: url.to_owned(),
        max_connections,
        connect_timeout,
        ping_timeout,
        operation_timeout,
        lock_timeout,
    })
}

fn parse_bounded_u32(
    value: Option<&String>,
    name: &'static str,
    default: u32,
    min: u32,
    max: u32,
) -> Result<u32, ConfigError> {
    let Some(value) = value else {
        return Ok(default);
    };
    let parsed = value
        .trim()
        .parse::<u32>()
        .map_err(|source| ConfigError::InvalidInteger { name, source })?;

    if !(min..=max).contains(&parsed) {
        return Err(ConfigError::OutOfRange {
            name,
            min: u64::from(min),
            max: u64::from(max),
        });
    }

    Ok(parsed)
}

fn parse_duration(
    value: Option<&String>,
    name: &'static str,
    default_ms: u64,
) -> Result<Duration, ConfigError> {
    parse_bounded_duration(value, name, default_ms, 1, MAX_DATABASE_TIMEOUT_MS)
}

fn parse_bounded_duration(
    value: Option<&String>,
    name: &'static str,
    default_ms: u64,
    min_ms: u64,
    max_ms: u64,
) -> Result<Duration, ConfigError> {
    let milliseconds = match value {
        Some(value) => value
            .trim()
            .parse::<u64>()
            .map_err(|source| ConfigError::InvalidInteger { name, source })?,
        None => default_ms,
    };

    if !(min_ms..=max_ms).contains(&milliseconds) {
        return Err(ConfigError::OutOfRange {
            name,
            min: min_ms,
            max: max_ms,
        });
    }

    Ok(Duration::from_millis(milliseconds))
}

fn parse_bounded_seconds(
    value: Option<&String>,
    name: &'static str,
    default_seconds: u64,
    min_seconds: u64,
    max_seconds: u64,
) -> Result<Duration, ConfigError> {
    let seconds = match value {
        Some(value) => value
            .trim()
            .parse::<u64>()
            .map_err(|source| ConfigError::InvalidInteger { name, source })?,
        None => default_seconds,
    };
    if !(min_seconds..=max_seconds).contains(&seconds) {
        return Err(ConfigError::OutOfRange {
            name,
            min: min_seconds,
            max: max_seconds,
        });
    }
    Ok(Duration::from_secs(seconds))
}

fn parse_allowed_origins(
    value: Option<&String>,
    production: bool,
) -> Result<Vec<String>, ConfigError> {
    let value = value
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .ok_or(ConfigError::Missing {
            name: ALLOWED_ORIGINS_KEY,
        })?;
    let mut origins = Vec::new();

    for (index, value) in value.split(',').enumerate() {
        let position = index + 1;
        let value = value.trim();
        let parsed = Url::parse(value).map_err(|_| ConfigError::InvalidOrigin {
            name: ALLOWED_ORIGINS_KEY,
            position,
        })?;

        let is_http = matches!(parsed.scheme(), "http" | "https");
        let is_bare_origin = parsed.host().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.path() == "/"
            && parsed.query().is_none()
            && parsed.fragment().is_none();
        if !is_http || !is_bare_origin {
            return Err(ConfigError::InvalidOrigin {
                name: ALLOWED_ORIGINS_KEY,
                position,
            });
        }
        if production && parsed.scheme() != "https" {
            return Err(ConfigError::InsecureProductionOrigin {
                name: ALLOWED_ORIGINS_KEY,
                position,
            });
        }

        let origin = parsed.origin().ascii_serialization();
        if !origins.contains(&origin) {
            origins.push(origin);
        }
    }

    Ok(origins)
}

fn parse_event_reminder_offsets(value: Option<&String>) -> Result<Vec<u32>, ConfigError> {
    let value = value.map_or(DEFAULT_EVENT_REMINDER_OFFSETS_MINUTES, String::as_str);
    let mut offsets = Vec::new();
    for raw in value.split(',') {
        let Ok(offset) = raw.trim().parse::<u32>() else {
            return Err(ConfigError::InvalidReminderOffsets {
                name: EVENT_REMINDER_OFFSETS_MINUTES_KEY,
                max: MAX_EVENT_REMINDER_OFFSET_MINUTES,
            });
        };
        if offset == 0
            || offset > MAX_EVENT_REMINDER_OFFSET_MINUTES
            || offsets.contains(&offset)
            || offsets.len() >= MAX_EVENT_REMINDER_OFFSETS
        {
            return Err(ConfigError::InvalidReminderOffsets {
                name: EVENT_REMINDER_OFFSETS_MINUTES_KEY,
                max: MAX_EVENT_REMINDER_OFFSET_MINUTES,
            });
        }
        offsets.push(offset);
    }
    offsets.sort_unstable_by(|left, right| right.cmp(left));
    Ok(offsets)
}

fn parse_commerce_api_key(value: Option<&String>) -> Result<Option<[u8; 32]>, ConfigError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if !(32..=256).contains(&value.len())
        || value.trim() != value
        || !value.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(ConfigError::InvalidSecret {
            name: COMMERCE_API_KEY,
        });
    }
    Ok(Some(Sha256::digest(value.as_bytes()).into()))
}

fn parse_bool(
    value: Option<&String>,
    name: &'static str,
    default: bool,
) -> Result<bool, ConfigError> {
    let Some(value) = value else {
        return Ok(default);
    };

    match value.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(ConfigError::InvalidBoolean { name }),
    }
}

fn parse_random_draws(value: Option<&String>) -> Result<bool, ConfigError> {
    match value.map(|value| value.trim()) {
        None | Some("") | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(_) => Err(ConfigError::InvalidBoolean {
            name: RANDOM_DRAWS_ENABLED_KEY,
        }),
    }
}

fn parse_workspace_slug(value: Option<&String>) -> Result<WorkspaceSlug, ConfigError> {
    let slug = value
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .ok_or(ConfigError::Missing {
            name: WORKSPACE_SLUG_KEY,
        })?;

    WorkspaceSlug::parse(slug).map_err(|_| ConfigError::InvalidWorkspaceSlug {
        name: WORKSPACE_SLUG_KEY,
    })
}

fn parse_public_site_base_url(
    value: Option<&String>,
    production: bool,
) -> Result<Url, ConfigError> {
    let value = value
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .ok_or(ConfigError::Missing {
            name: PUBLIC_SITE_BASE_URL_KEY,
        })?;
    let parsed = Url::parse(value).map_err(|_| ConfigError::InvalidPublicSiteBaseUrl {
        name: PUBLIC_SITE_BASE_URL_KEY,
    })?;

    let is_http = matches!(parsed.scheme(), "http" | "https");
    let is_bare_origin = parsed.host().is_some()
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.path() == "/"
        && parsed.query().is_none()
        && parsed.fragment().is_none();
    if !is_http || !is_bare_origin {
        return Err(ConfigError::InvalidPublicSiteBaseUrl {
            name: PUBLIC_SITE_BASE_URL_KEY,
        });
    }
    if production && parsed.scheme() != "https" {
        return Err(ConfigError::InsecureProductionSiteUrl {
            name: PUBLIC_SITE_BASE_URL_KEY,
        });
    }

    Ok(parsed)
}

fn parse_country_code(value: Option<&String>) -> Result<CountryCode, ConfigError> {
    let country_code = value.map_or(DEFAULT_COUNTRY_CODE, String::as_str).trim();
    CountryCode::parse(country_code).map_err(|_| ConfigError::InvalidCountryCode {
        name: DEFAULT_COUNTRY_CODE_KEY,
    })
}

fn parse_admission_security(
    values: &HashMap<String, String>,
    production: bool,
) -> Result<AdmissionSecurityConfig, ConfigError> {
    let admin_api_key_sha256 =
        parse_optional_secret_hash(values.get(ADMIN_API_KEY_KEY), ADMIN_API_KEY_KEY)?;
    let staff_api_key_sha256 =
        parse_optional_secret_hash(values.get(STAFF_API_KEY_KEY), STAFF_API_KEY_KEY)?;
    let previous_admin_api_key_sha256 = parse_optional_secret_hash(
        values.get(PREVIOUS_ADMIN_API_KEY_KEY),
        PREVIOUS_ADMIN_API_KEY_KEY,
    )?;
    let previous_staff_api_key_sha256 = parse_optional_secret_hash(
        values.get(PREVIOUS_STAFF_API_KEY_KEY),
        PREVIOUS_STAFF_API_KEY_KEY,
    )?;
    let qr_signing_key =
        parse_optional_secret_hash(values.get(QR_SIGNING_SECRET_KEY), QR_SIGNING_SECRET_KEY)?;
    if admin_api_key_sha256.is_some() && admin_api_key_sha256 == staff_api_key_sha256 {
        return Err(ConfigError::InvalidSecret {
            name: STAFF_API_KEY_KEY,
        });
    }
    for (previous, name) in [
        (
            previous_admin_api_key_sha256 == admin_api_key_sha256
                && previous_admin_api_key_sha256.is_some(),
            PREVIOUS_ADMIN_API_KEY_KEY,
        ),
        (
            previous_staff_api_key_sha256 == staff_api_key_sha256
                && previous_staff_api_key_sha256.is_some(),
            PREVIOUS_STAFF_API_KEY_KEY,
        ),
    ] {
        if previous {
            return Err(ConfigError::InvalidSecret { name });
        }
    }
    if production {
        for (name, value) in [
            (ADMIN_API_KEY_KEY, admin_api_key_sha256),
            (STAFF_API_KEY_KEY, staff_api_key_sha256),
            (QR_SIGNING_SECRET_KEY, qr_signing_key),
        ] {
            if value.is_none() {
                return Err(ConfigError::MissingProductionAdmissionSecret { name });
            }
        }
    }
    let admin_member_email = parse_member_email(
        values.get(ADMIN_MEMBER_EMAIL_KEY),
        ADMIN_MEMBER_EMAIL_KEY,
        DEFAULT_ADMIN_MEMBER_EMAIL,
    )?;
    let staff_member_email = parse_member_email(
        values.get(STAFF_MEMBER_EMAIL_KEY),
        STAFF_MEMBER_EMAIL_KEY,
        DEFAULT_STAFF_MEMBER_EMAIL,
    )?;
    if admin_member_email == staff_member_email {
        return Err(ConfigError::InvalidMemberEmail {
            name: STAFF_MEMBER_EMAIL_KEY,
        });
    }
    let qr_ttl = parse_bounded_seconds(
        values.get(QR_TTL_SECONDS_KEY),
        QR_TTL_SECONDS_KEY,
        DEFAULT_QR_TTL_SECONDS,
        MIN_QR_TTL_SECONDS,
        MAX_QR_TTL_SECONDS,
    )?;
    Ok(AdmissionSecurityConfig {
        admin_api_key_sha256,
        staff_api_key_sha256,
        previous_admin_api_key_sha256,
        previous_staff_api_key_sha256,
        qr_signing_key,
        admin_member_email,
        staff_member_email,
        qr_ttl,
    })
}

pub(super) fn parse_optional_secret_hash(
    value: Option<&String>,
    name: &'static str,
) -> Result<Option<[u8; 32]>, ConfigError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if !(32..=256).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(ConfigError::InvalidSecret { name });
    }
    Ok(Some(Sha256::digest(value.as_bytes()).into()))
}

fn parse_response_encryption_key(
    value: Option<&String>,
    production: bool,
) -> Result<SensitiveResponseKey, ConfigError> {
    let secret = match value {
        Some(value) => value.as_str(),
        None if production => {
            return Err(ConfigError::Missing {
                name: RESPONSE_ENCRYPTION_SECRET_KEY,
            });
        }
        None => LOCAL_RESPONSE_ENCRYPTION_SECRET,
    };
    if !(32..=256).contains(&secret.len()) || !secret.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(ConfigError::InvalidSecret {
            name: RESPONSE_ENCRYPTION_SECRET_KEY,
        });
    }
    if production
        && matches!(
            secret,
            LOCAL_RESPONSE_ENCRYPTION_SECRET | "REPLACE_RESPONSE_ENCRYPTION_SECRET"
        )
    {
        return Err(ConfigError::InvalidSecret {
            name: RESPONSE_ENCRYPTION_SECRET_KEY,
        });
    }
    Ok(SensitiveResponseKey::derive_from_secret(secret.as_bytes()))
}

fn parse_previous_response_encryption_key(
    value: Option<&String>,
    production: bool,
) -> Result<Option<SensitiveResponseKey>, ConfigError> {
    let Some(secret) = value.map(String::as_str).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if !(32..=256).contains(&secret.len()) || !secret.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(ConfigError::InvalidSecret {
            name: PREVIOUS_RESPONSE_ENCRYPTION_SECRET_KEY,
        });
    }
    if production
        && matches!(
            secret,
            LOCAL_RESPONSE_ENCRYPTION_SECRET | "REPLACE_RESPONSE_ENCRYPTION_SECRET"
        )
    {
        return Err(ConfigError::InvalidSecret {
            name: PREVIOUS_RESPONSE_ENCRYPTION_SECRET_KEY,
        });
    }
    Ok(Some(SensitiveResponseKey::derive_from_secret(
        secret.as_bytes(),
    )))
}

fn parse_member_email(
    value: Option<&String>,
    name: &'static str,
    default: &'static str,
) -> Result<String, ConfigError> {
    NormalizedEmail::parse(value.map(String::as_str).unwrap_or(default))
        .map(NormalizedEmail::into_inner)
        .map_err(|_| ConfigError::InvalidMemberEmail { name })
}

fn parse_rate_limit(values: &HashMap<String, String>) -> Result<RateLimitConfig, ConfigError> {
    Ok(RateLimitConfig {
        enabled: parse_bool(
            values.get(RATE_LIMIT_ENABLED_KEY),
            RATE_LIMIT_ENABLED_KEY,
            true,
        )?,
        public_auth_per_minute: parse_bounded_u32(
            values.get(RATE_LIMIT_PUBLIC_AUTH_PER_MINUTE_KEY),
            RATE_LIMIT_PUBLIC_AUTH_PER_MINUTE_KEY,
            DEFAULT_RATE_LIMIT_PUBLIC_AUTH_PER_MINUTE,
            0,
            MAX_RATE_LIMIT_PER_MINUTE,
        )?,
        privileged_per_minute: parse_bounded_u32(
            values.get(RATE_LIMIT_PRIVILEGED_PER_MINUTE_KEY),
            RATE_LIMIT_PRIVILEGED_PER_MINUTE_KEY,
            DEFAULT_RATE_LIMIT_PRIVILEGED_PER_MINUTE,
            0,
            MAX_RATE_LIMIT_PER_MINUTE,
        )?,
        general_per_minute: parse_bounded_u32(
            values.get(RATE_LIMIT_GENERAL_PER_MINUTE_KEY),
            RATE_LIMIT_GENERAL_PER_MINUTE_KEY,
            DEFAULT_RATE_LIMIT_GENERAL_PER_MINUTE,
            1,
            MAX_RATE_LIMIT_PER_MINUTE,
        )?,
    })
}
