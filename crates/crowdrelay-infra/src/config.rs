//! Environment-backed application configuration.
//!
//! All settings are loaded from process environment variables with safe,
//! bounded defaults. Production-specific safety checks enforce HTTPS origins
//! and required secrets; weighted draws remain an explicit opt-in kill switch.

use std::{
    collections::HashMap,
    env, fmt,
    net::{AddrParseError, SocketAddr},
    num::ParseIntError,
    str::FromStr,
    time::Duration,
};

use crowdrelay_domain::{CountryCode, NormalizedEmail, WorkspaceSlug};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgConnectOptions;
use thiserror::Error;
use url::Url;

use crate::sensitive_response::SensitiveResponseKey;

const ENVIRONMENT_KEY: &str = "CROWDRELAY_ENV";
const BIND_ADDR_KEY: &str = "CROWDRELAY_BIND_ADDR";
const DATABASE_URL_KEY: &str = "CROWDRELAY_DATABASE_URL";
const DATABASE_MAX_CONNECTIONS_KEY: &str = "CROWDRELAY_DATABASE_MAX_CONNECTIONS";
const DATABASE_CONNECT_TIMEOUT_MS_KEY: &str = "CROWDRELAY_DATABASE_CONNECT_TIMEOUT_MS";
const DATABASE_PING_TIMEOUT_MS_KEY: &str = "CROWDRELAY_DATABASE_PING_TIMEOUT_MS";
const DATABASE_OPERATION_TIMEOUT_MS_KEY: &str = "CROWDRELAY_DATABASE_OPERATION_TIMEOUT_MS";
const DATABASE_LOCK_TIMEOUT_MS_KEY: &str = "CROWDRELAY_DATABASE_LOCK_TIMEOUT_MS";
const ALLOWED_ORIGINS_KEY: &str = "CROWDRELAY_ALLOWED_ORIGINS";
const RANDOM_DRAWS_ENABLED_KEY: &str = "CROWDRELAY_RANDOM_DRAWS_ENABLED";
const WORKSPACE_SLUG_KEY: &str = "CROWDRELAY_WORKSPACE_SLUG";
const PUBLIC_SITE_BASE_URL_KEY: &str = "CROWDRELAY_PUBLIC_SITE_BASE_URL";
const DEFAULT_COUNTRY_CODE_KEY: &str = "CROWDRELAY_DEFAULT_COUNTRY_CODE";
const REDIRECT_REFRESH_INTERVAL_MS_KEY: &str = "CROWDRELAY_REDIRECT_REFRESH_INTERVAL_MS";
const CLICK_CHANNEL_CAPACITY_KEY: &str = "CROWDRELAY_CLICK_CHANNEL_CAPACITY";
const CLICK_BATCH_SIZE_KEY: &str = "CROWDRELAY_CLICK_BATCH_SIZE";
const CLICK_FLUSH_INTERVAL_MS_KEY: &str = "CROWDRELAY_CLICK_FLUSH_INTERVAL_MS";
const COMMERCE_API_KEY: &str = "CROWDRELAY_COMMERCE_API_KEY";
const EVENT_REMINDER_OFFSETS_MINUTES_KEY: &str = "CROWDRELAY_EVENT_REMINDER_OFFSETS_MINUTES";
const EVENT_REMINDER_POLL_INTERVAL_MS_KEY: &str = "CROWDRELAY_EVENT_REMINDER_POLL_INTERVAL_MS";
const ADMIN_API_KEY_KEY: &str = "CROWDRELAY_ADMIN_API_KEY";
const STAFF_API_KEY_KEY: &str = "CROWDRELAY_STAFF_API_KEY";
const QR_SIGNING_SECRET_KEY: &str = "CROWDRELAY_QR_SIGNING_SECRET";
const ADMIN_MEMBER_EMAIL_KEY: &str = "CROWDRELAY_ADMIN_MEMBER_EMAIL";
const STAFF_MEMBER_EMAIL_KEY: &str = "CROWDRELAY_STAFF_MEMBER_EMAIL";
const QR_TTL_SECONDS_KEY: &str = "CROWDRELAY_QR_TTL_SECONDS";
const REQUIRE_DOUBLE_OPT_IN_KEY: &str = "CROWDRELAY_REQUIRE_DOUBLE_OPT_IN";
const RESPONSE_ENCRYPTION_SECRET_KEY: &str = "CROWDRELAY_RESPONSE_ENCRYPTION_SECRET";
const PREVIOUS_RESPONSE_ENCRYPTION_SECRET_KEY: &str =
    "CROWDRELAY_PREVIOUS_RESPONSE_ENCRYPTION_SECRET";

const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8080";
const DEFAULT_DATABASE_MAX_CONNECTIONS: u32 = 10;
const MAX_DATABASE_CONNECTIONS: u32 = 100;
const DEFAULT_DATABASE_CONNECT_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_DATABASE_PING_TIMEOUT_MS: u64 = 2_000;
const DEFAULT_DATABASE_OPERATION_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_DATABASE_LOCK_TIMEOUT_MS: u64 = 1_000;
const MAX_DATABASE_TIMEOUT_MS: u64 = 60_000;
const MAX_DATABASE_LOCK_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_COUNTRY_CODE: &str = "PL";
const DEFAULT_REDIRECT_REFRESH_INTERVAL_MS: u64 = 30_000;
const MIN_REDIRECT_REFRESH_INTERVAL_MS: u64 = 1_000;
const MAX_REDIRECT_REFRESH_INTERVAL_MS: u64 = 600_000;
const DEFAULT_CLICK_CHANNEL_CAPACITY: u32 = 4_096;
const MAX_CLICK_CHANNEL_CAPACITY: u32 = 65_536;
const DEFAULT_CLICK_BATCH_SIZE: u32 = 250;
const MAX_CLICK_BATCH_SIZE: u32 = 1_000;
const DEFAULT_CLICK_FLUSH_INTERVAL_MS: u64 = 500;
const MIN_CLICK_FLUSH_INTERVAL_MS: u64 = 10;
const MAX_CLICK_FLUSH_INTERVAL_MS: u64 = 60_000;
const DEFAULT_EVENT_REMINDER_OFFSETS_MINUTES: &str = "1440,120";
const DEFAULT_EVENT_REMINDER_POLL_INTERVAL_MS: u64 = 30_000;
const MIN_EVENT_REMINDER_POLL_INTERVAL_MS: u64 = 1_000;
const MAX_EVENT_REMINDER_POLL_INTERVAL_MS: u64 = 600_000;
const MAX_EVENT_REMINDER_OFFSETS: usize = 8;
const MAX_EVENT_REMINDER_OFFSET_MINUTES: u32 = 43_200;
const DEFAULT_ADMIN_MEMBER_EMAIL: &str = "admin@example.invalid";
const DEFAULT_STAFF_MEMBER_EMAIL: &str = "staff@example.invalid";
const DEFAULT_QR_TTL_SECONDS: u64 = 30;
const MIN_QR_TTL_SECONDS: u64 = 10;
const MAX_QR_TTL_SECONDS: u64 = 120;
const DEFAULT_REQUIRE_DOUBLE_OPT_IN: bool = true;
const LOCAL_RESPONSE_ENCRYPTION_SECRET: &str = "crowdrelay-local-response-encryption-only";

const KNOWN_KEYS: &[&str] = &[
    ENVIRONMENT_KEY,
    BIND_ADDR_KEY,
    DATABASE_URL_KEY,
    DATABASE_MAX_CONNECTIONS_KEY,
    DATABASE_CONNECT_TIMEOUT_MS_KEY,
    DATABASE_PING_TIMEOUT_MS_KEY,
    DATABASE_OPERATION_TIMEOUT_MS_KEY,
    DATABASE_LOCK_TIMEOUT_MS_KEY,
    ALLOWED_ORIGINS_KEY,
    RANDOM_DRAWS_ENABLED_KEY,
    WORKSPACE_SLUG_KEY,
    PUBLIC_SITE_BASE_URL_KEY,
    DEFAULT_COUNTRY_CODE_KEY,
    REDIRECT_REFRESH_INTERVAL_MS_KEY,
    CLICK_CHANNEL_CAPACITY_KEY,
    CLICK_BATCH_SIZE_KEY,
    CLICK_FLUSH_INTERVAL_MS_KEY,
    COMMERCE_API_KEY,
    EVENT_REMINDER_OFFSETS_MINUTES_KEY,
    EVENT_REMINDER_POLL_INTERVAL_MS_KEY,
    ADMIN_API_KEY_KEY,
    STAFF_API_KEY_KEY,
    QR_SIGNING_SECRET_KEY,
    ADMIN_MEMBER_EMAIL_KEY,
    STAFF_MEMBER_EMAIL_KEY,
    QR_TTL_SECONDS_KEY,
    REQUIRE_DOUBLE_OPT_IN_KEY,
    RESPONSE_ENCRYPTION_SECRET_KEY,
    PREVIOUS_RESPONSE_ENCRYPTION_SECRET_KEY,
];

/// Runtime configuration shared by the API and worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub environment: Environment,
    pub bind_addr: SocketAddr,
    pub database: DatabaseConfig,
    pub allowed_origins: Vec<String>,
    pub random_draws_enabled: bool,
    pub workspace_slug: WorkspaceSlug,
    pub public_site_base_url: Url,
    pub default_country_code: CountryCode,
    pub redirect_refresh_interval: Duration,
    pub click_buffer: ClickBufferConfig,
    pub commerce_api_key_sha256: Option<[u8; 32]>,
    pub event_reminder_offsets_minutes: Vec<u32>,
    pub event_reminder_poll_interval: Duration,
    pub admission_security: AdmissionSecurityConfig,
    /// Derived AEAD key for sensitive idempotency response replay.
    pub response_encryption_key: SensitiveResponseKey,
    /// Optional immediately preceding AEAD key used during bounded rotation.
    pub previous_response_encryption_key: Option<SensitiveResponseKey>,
    /// Requires inbox ownership confirmation before a fan becomes active.
    pub require_double_opt_in: bool,
}

impl Config {
    /// Loads CrowdRelay settings from the process environment.
    ///
    /// Unknown variables are ignored. A non-Unicode value is rejected only
    /// when it belongs to a CrowdRelay setting understood by this version.
    pub fn from_env() -> Result<Self, ConfigError> {
        let mut values = Vec::with_capacity(KNOWN_KEYS.len());

        for (key, value) in env::vars_os() {
            let Ok(key) = key.into_string() else {
                continue;
            };

            if !KNOWN_KEYS.contains(&key.as_str()) {
                continue;
            }

            let value = value
                .into_string()
                .map_err(|_| ConfigError::NonUnicode { name: key.clone() })?;
            values.push((key, value));
        }

        Self::from_values(values)
    }

    /// Parses configuration from key-value pairs.
    ///
    /// This is useful for deterministic tests and for launchers that source
    /// configuration somewhere other than the process environment.
    pub fn from_values<I, K, V>(values: I) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let values: HashMap<String, String> = values
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();

        let environment = parse_environment(values.get(ENVIRONMENT_KEY))?;
        let bind_addr = parse_socket_addr(values.get(BIND_ADDR_KEY))?;
        let database = parse_database_config(&values)?;
        let allowed_origins =
            parse_allowed_origins(values.get(ALLOWED_ORIGINS_KEY), environment.is_production())?;
        let random_draws_enabled = parse_random_draws(values.get(RANDOM_DRAWS_ENABLED_KEY))?;
        let workspace_slug = parse_workspace_slug(values.get(WORKSPACE_SLUG_KEY))?;
        let public_site_base_url = parse_public_site_base_url(
            values.get(PUBLIC_SITE_BASE_URL_KEY),
            environment.is_production(),
        )?;
        let default_country_code = parse_country_code(values.get(DEFAULT_COUNTRY_CODE_KEY))?;
        let redirect_refresh_interval = parse_bounded_duration(
            values.get(REDIRECT_REFRESH_INTERVAL_MS_KEY),
            REDIRECT_REFRESH_INTERVAL_MS_KEY,
            DEFAULT_REDIRECT_REFRESH_INTERVAL_MS,
            MIN_REDIRECT_REFRESH_INTERVAL_MS,
            MAX_REDIRECT_REFRESH_INTERVAL_MS,
        )?;
        let click_buffer = parse_click_buffer_config(&values)?;
        let commerce_api_key_sha256 = parse_commerce_api_key(values.get(COMMERCE_API_KEY))?;
        let event_reminder_offsets_minutes =
            parse_event_reminder_offsets(values.get(EVENT_REMINDER_OFFSETS_MINUTES_KEY))?;
        let event_reminder_poll_interval = parse_bounded_duration(
            values.get(EVENT_REMINDER_POLL_INTERVAL_MS_KEY),
            EVENT_REMINDER_POLL_INTERVAL_MS_KEY,
            DEFAULT_EVENT_REMINDER_POLL_INTERVAL_MS,
            MIN_EVENT_REMINDER_POLL_INTERVAL_MS,
            MAX_EVENT_REMINDER_POLL_INTERVAL_MS,
        )?;
        let admission_security = parse_admission_security(&values, environment.is_production())?;
        let response_encryption_key = parse_response_encryption_key(
            values.get(RESPONSE_ENCRYPTION_SECRET_KEY),
            environment.is_production(),
        )?;
        let previous_response_encryption_key = parse_previous_response_encryption_key(
            values.get(PREVIOUS_RESPONSE_ENCRYPTION_SECRET_KEY),
            environment.is_production(),
        )?;
        if previous_response_encryption_key.as_ref() == Some(&response_encryption_key) {
            return Err(ConfigError::InvalidSecret {
                name: PREVIOUS_RESPONSE_ENCRYPTION_SECRET_KEY,
            });
        }
        let require_double_opt_in = parse_bool(
            values.get(REQUIRE_DOUBLE_OPT_IN_KEY),
            REQUIRE_DOUBLE_OPT_IN_KEY,
            DEFAULT_REQUIRE_DOUBLE_OPT_IN,
        )?;

        Ok(Self {
            environment,
            bind_addr,
            database,
            allowed_origins,
            random_draws_enabled,
            workspace_slug,
            public_site_base_url,
            default_country_code,
            redirect_refresh_interval,
            click_buffer,
            commerce_api_key_sha256,
            event_reminder_offsets_minutes,
            event_reminder_poll_interval,
            admission_security,
            response_encryption_key,
            previous_response_encryption_key,
            require_double_opt_in,
        })
    }
}

/// The deployment environment. Production-specific safety checks can key off
/// this value without relying on ad-hoc string comparisons.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Environment {
    /// Local development (default).
    #[default]
    Development,
    /// Automated test environment.
    Test,
    /// Production with enforced safety checks.
    Production,
}

impl Environment {
    /// Returns whether production-only safety checks must be enforced.
    #[must_use]
    pub const fn is_production(self) -> bool {
        matches!(self, Self::Production)
    }
}

impl fmt::Display for Environment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Development => "development",
            Self::Test => "test",
            Self::Production => "production",
        };
        formatter.write_str(value)
    }
}

/// PostgreSQL pool and readiness timeouts.
#[derive(Clone, PartialEq, Eq)]
pub struct DatabaseConfig {
    /// PostgreSQL connection URL. Its `Debug` representation is always redacted.
    pub url: String,
    /// Maximum number of connections in the pool.
    pub max_connections: u32,
    /// Timeout for establishing a new connection.
    pub connect_timeout: Duration,
    /// Deadline for the readiness ping.
    pub ping_timeout: Duration,
    /// Timeout for individual queries.
    pub operation_timeout: Duration,
    /// PostgreSQL `lock_timeout` for transaction-level advisory locks.
    pub lock_timeout: Duration,
}

/// Fixed-capacity click analytics buffer and batcher settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClickBufferConfig {
    /// Maximum number of click events buffered before backpressure.
    pub capacity: usize,
    /// Number of clicks persisted per batch flush.
    pub batch_size: usize,
    /// Maximum interval between batch flushes.
    pub flush_interval: Duration,
}

/// Hashed operator credentials and rotating QR signing configuration.
#[derive(Clone, PartialEq, Eq)]
pub struct AdmissionSecurityConfig {
    /// SHA-256 hash of the admin API key, if configured.
    pub admin_api_key_sha256: Option<[u8; 32]>,
    /// SHA-256 hash of the gate staff API key, if configured.
    pub staff_api_key_sha256: Option<[u8; 32]>,
    /// HMAC key for signing rotating admission QR payloads, if configured.
    pub qr_signing_key: Option<[u8; 32]>,
    /// Normalized email of the default admin member.
    pub admin_member_email: String,
    /// Normalized email of the default gate staff member.
    pub staff_member_email: String,
    /// Time-to-live for rotating admission QR payloads.
    pub qr_ttl: Duration,
}

impl fmt::Debug for AdmissionSecurityConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdmissionSecurityConfig")
            .field(
                "admin_api_key_sha256",
                &self.admin_api_key_sha256.map(|_| "[REDACTED]"),
            )
            .field(
                "staff_api_key_sha256",
                &self.staff_api_key_sha256.map(|_| "[REDACTED]"),
            )
            .field("qr_signing_key", &self.qr_signing_key.map(|_| "[REDACTED]"))
            .field("admin_member_email", &"[REDACTED]")
            .field("staff_member_email", &"[REDACTED]")
            .field("qr_ttl", &self.qr_ttl)
            .finish()
    }
}

impl fmt::Debug for DatabaseConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DatabaseConfig")
            .field("url", &"[REDACTED]")
            .field("max_connections", &self.max_connections)
            .field("connect_timeout", &self.connect_timeout)
            .field("ping_timeout", &self.ping_timeout)
            .field("operation_timeout", &self.operation_timeout)
            .field("lock_timeout", &self.lock_timeout)
            .finish()
    }
}

/// Error returned when configuration parsing fails.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// A required environment variable was missing.
    #[error("required environment variable {name} is missing")]
    Missing { name: &'static str },

    /// An environment variable contained non-UTF-8 data.
    #[error("environment variable {name} is not valid Unicode")]
    NonUnicode { name: String },

    /// The environment name was not recognized.
    #[error("environment variable {name} contains an unsupported environment name")]
    InvalidEnvironment { name: &'static str },

    /// The bind address was not a valid socket address.
    #[error("environment variable {name} is not a valid socket address")]
    InvalidSocketAddress {
        name: &'static str,
        #[source]
        source: AddrParseError,
    },

    /// The database URL was not a valid PostgreSQL URL.
    #[error("environment variable {name} is not a valid PostgreSQL URL")]
    InvalidDatabaseUrl { name: &'static str },

    /// An integer-valued setting could not be parsed.
    #[error("environment variable {name} is not an unsigned integer")]
    InvalidInteger {
        name: &'static str,
        #[source]
        source: ParseIntError,
    },

    /// A numeric setting was outside its allowed range.
    #[error("environment variable {name} must be between {min} and {max}")]
    OutOfRange {
        name: &'static str,
        min: u64,
        max: u64,
    },

    /// A boolean setting was not `true` or `false`.
    #[error("environment variable {name} must be exactly `true` or `false`")]
    InvalidBoolean { name: &'static str },

    /// A CORS origin was not a valid HTTP origin.
    #[error("origin at position {position} in {name} is not a valid HTTP origin")]
    InvalidOrigin { name: &'static str, position: usize },

    /// A CORS origin used HTTP in production.
    #[error("origin at position {position} in {name} must use HTTPS in production")]
    InsecureProductionOrigin { name: &'static str, position: usize },

    /// A secret (API key or signing key) failed length or character validation.
    #[error("environment variable {name} must contain 32 to 256 safe visible ASCII characters")]
    InvalidSecret { name: &'static str },

    /// The workspace slug failed validation.
    #[error("environment variable {name} is not a valid workspace slug")]
    InvalidWorkspaceSlug { name: &'static str },

    /// The public site base URL was not a valid HTTP URL.
    #[error("environment variable {name} is not a valid public HTTP base URL")]
    InvalidPublicSiteBaseUrl { name: &'static str },

    /// The public site base URL used HTTP in production.
    #[error("environment variable {name} must use HTTPS in production")]
    InsecureProductionSiteUrl { name: &'static str },

    /// The country code was not a valid ISO 3166-1 alpha-2 code.
    #[error("environment variable {name} must be an uppercase ISO 3166-1 alpha-2 code")]
    InvalidCountryCode { name: &'static str },

    /// The click batch size exceeded the channel capacity.
    #[error("{batch_name} cannot exceed {capacity_name}")]
    BatchExceedsCapacity {
        batch_name: &'static str,
        capacity_name: &'static str,
    },

    /// The event reminder offsets were invalid, non-unique, or too numerous.
    #[error(
        "environment variable {name} must contain unique comma-separated reminder offsets between 1 and {max} minutes"
    )]
    InvalidReminderOffsets { name: &'static str, max: u32 },

    /// A required admission secret was missing in production.
    #[error("production admission API requires {name}")]
    MissingProductionAdmissionSecret { name: &'static str },

    /// A member email address failed validation.
    #[error("environment variable {name} must contain a valid normalized email address")]
    InvalidMemberEmail { name: &'static str },

    /// The database lock timeout exceeded the operation timeout.
    #[error(
        "CROWDRELAY_DATABASE_LOCK_TIMEOUT_MS cannot exceed \
         CROWDRELAY_DATABASE_OPERATION_TIMEOUT_MS"
    )]
    LockTimeoutExceedsOperationTimeout,
}

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

fn parse_click_buffer_config(
    values: &HashMap<String, String>,
) -> Result<ClickBufferConfig, ConfigError> {
    let capacity = parse_bounded_u32(
        values.get(CLICK_CHANNEL_CAPACITY_KEY),
        CLICK_CHANNEL_CAPACITY_KEY,
        DEFAULT_CLICK_CHANNEL_CAPACITY,
        1,
        MAX_CLICK_CHANNEL_CAPACITY,
    )?;
    let batch_size = parse_bounded_u32(
        values.get(CLICK_BATCH_SIZE_KEY),
        CLICK_BATCH_SIZE_KEY,
        DEFAULT_CLICK_BATCH_SIZE,
        1,
        MAX_CLICK_BATCH_SIZE,
    )?;
    if batch_size > capacity {
        return Err(ConfigError::BatchExceedsCapacity {
            batch_name: CLICK_BATCH_SIZE_KEY,
            capacity_name: CLICK_CHANNEL_CAPACITY_KEY,
        });
    }
    let flush_interval = parse_bounded_duration(
        values.get(CLICK_FLUSH_INTERVAL_MS_KEY),
        CLICK_FLUSH_INTERVAL_MS_KEY,
        DEFAULT_CLICK_FLUSH_INTERVAL_MS,
        MIN_CLICK_FLUSH_INTERVAL_MS,
        MAX_CLICK_FLUSH_INTERVAL_MS,
    )?;

    Ok(ClickBufferConfig {
        capacity: capacity as usize,
        batch_size: batch_size as usize,
        flush_interval,
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
    let qr_signing_key =
        parse_optional_secret_hash(values.get(QR_SIGNING_SECRET_KEY), QR_SIGNING_SECRET_KEY)?;
    if admin_api_key_sha256.is_some() && admin_api_key_sha256 == staff_api_key_sha256 {
        return Err(ConfigError::InvalidSecret {
            name: STAFF_API_KEY_KEY,
        });
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
        qr_signing_key,
        admin_member_email,
        staff_member_email,
        qr_ttl,
    })
}

fn parse_optional_secret_hash(
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
