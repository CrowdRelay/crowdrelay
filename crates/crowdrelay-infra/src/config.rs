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

mod click;
mod meta;
mod push;
mod team;

use click::parse_click_buffer_config;
pub use meta::{AdConversionConfig, BandsintownConversionConfig, GoogleAdsConfig, MetaCapiConfig};
pub use push::PushPublicConfig;
pub use team::TeamOperationsConfig;
use team::{
    VIRYA_TEAM_MEMBER_1_EMAIL_KEY, VIRYA_TEAM_MEMBER_2_EMAIL_KEY, VIRYA_TEAM_MEMBER_3_EMAIL_KEY,
    VIRYA_TEAM_MEMBER_4_EMAIL_KEY, VIRYA_TEAM_MEMBER_5_EMAIL_KEY, parse_team_operations,
    validate_production_team_contacts,
};

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
const CONTROL_PLANE_AREA_API_KEY: &str = "CROWDRELAY_CONTROL_PLANE_AREA_API_KEY";
const CONTROL_PLANE_API_KEY: &str = "CROWDRELAY_CONTROL_PLANE_API_KEY";
const PREVIOUS_ADMIN_API_KEY_KEY: &str = "CROWDRELAY_PREVIOUS_ADMIN_API_KEY";
const PREVIOUS_STAFF_API_KEY_KEY: &str = "CROWDRELAY_PREVIOUS_STAFF_API_KEY";
const PREVIOUS_COMMERCE_API_KEY: &str = "CROWDRELAY_PREVIOUS_COMMERCE_API_KEY";
const PREVIOUS_CONTROL_PLANE_AREA_API_KEY: &str = "CROWDRELAY_PREVIOUS_CONTROL_PLANE_AREA_API_KEY";
const PREVIOUS_CONTROL_PLANE_API_KEY: &str = "CROWDRELAY_PREVIOUS_CONTROL_PLANE_API_KEY";
const EVENT_REMINDER_OFFSETS_MINUTES_KEY: &str = "CROWDRELAY_EVENT_REMINDER_OFFSETS_MINUTES";
const EVENT_REMINDER_POLL_INTERVAL_MS_KEY: &str = "CROWDRELAY_EVENT_REMINDER_POLL_INTERVAL_MS";
const AUTOPILOT_ENABLED_KEY: &str = "CROWDRELAY_AUTOPILOT_ENABLED";
const AUTOPILOT_POLL_INTERVAL_MS_KEY: &str = "CROWDRELAY_AUTOPILOT_POLL_INTERVAL_MS";
const AGENT_OUTCOMES_ENABLED_KEY: &str = "CROWDRELAY_AGENT_OUTCOMES_ENABLED";
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
const RATE_LIMIT_ENABLED_KEY: &str = "CROWDRELAY_RATE_LIMIT_ENABLED";
const RATE_LIMIT_PUBLIC_AUTH_PER_MINUTE_KEY: &str = "CROWDRELAY_RATE_LIMIT_PUBLIC_AUTH_PER_MINUTE";
const RATE_LIMIT_PRIVILEGED_PER_MINUTE_KEY: &str = "CROWDRELAY_RATE_LIMIT_PRIVILEGED_PER_MINUTE";
const RATE_LIMIT_GENERAL_PER_MINUTE_KEY: &str = "CROWDRELAY_RATE_LIMIT_GENERAL_PER_MINUTE";

const DEFAULT_RATE_LIMIT_PUBLIC_AUTH_PER_MINUTE: u32 = 30;
const DEFAULT_RATE_LIMIT_PRIVILEGED_PER_MINUTE: u32 = 120;
const DEFAULT_RATE_LIMIT_GENERAL_PER_MINUTE: u32 = 600;
const MAX_RATE_LIMIT_PER_MINUTE: u32 = 100_000;

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
const DEFAULT_AUTOPILOT_POLL_INTERVAL_MS: u64 = 300_000;
const MIN_AUTOPILOT_POLL_INTERVAL_MS: u64 = 60_000;
const MAX_AUTOPILOT_POLL_INTERVAL_MS: u64 = 3_600_000;
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
    CONTROL_PLANE_AREA_API_KEY,
    CONTROL_PLANE_API_KEY,
    PREVIOUS_ADMIN_API_KEY_KEY,
    PREVIOUS_STAFF_API_KEY_KEY,
    PREVIOUS_COMMERCE_API_KEY,
    PREVIOUS_CONTROL_PLANE_AREA_API_KEY,
    PREVIOUS_CONTROL_PLANE_API_KEY,
    EVENT_REMINDER_OFFSETS_MINUTES_KEY,
    EVENT_REMINDER_POLL_INTERVAL_MS_KEY,
    AUTOPILOT_ENABLED_KEY,
    AUTOPILOT_POLL_INTERVAL_MS_KEY,
    AGENT_OUTCOMES_ENABLED_KEY,
    ADMIN_API_KEY_KEY,
    STAFF_API_KEY_KEY,
    QR_SIGNING_SECRET_KEY,
    ADMIN_MEMBER_EMAIL_KEY,
    STAFF_MEMBER_EMAIL_KEY,
    VIRYA_TEAM_MEMBER_1_EMAIL_KEY,
    VIRYA_TEAM_MEMBER_2_EMAIL_KEY,
    VIRYA_TEAM_MEMBER_3_EMAIL_KEY,
    VIRYA_TEAM_MEMBER_4_EMAIL_KEY,
    VIRYA_TEAM_MEMBER_5_EMAIL_KEY,
    QR_TTL_SECONDS_KEY,
    REQUIRE_DOUBLE_OPT_IN_KEY,
    RESPONSE_ENCRYPTION_SECRET_KEY,
    PREVIOUS_RESPONSE_ENCRYPTION_SECRET_KEY,
    RATE_LIMIT_ENABLED_KEY,
    RATE_LIMIT_PUBLIC_AUTH_PER_MINUTE_KEY,
    RATE_LIMIT_PRIVILEGED_PER_MINUTE_KEY,
    RATE_LIMIT_GENERAL_PER_MINUTE_KEY,
    push::PUSH_DELIVERY_ENABLED_KEY,
    push::WEB_PUSH_VAPID_PUBLIC_KEY,
    push::FCM_PROJECT_ID_KEY,
    meta::META_CAPI_ENABLED_KEY,
    meta::META_PIXEL_ID_KEY,
    meta::META_CAPI_ACCESS_TOKEN_KEY,
    meta::META_CAPI_API_VERSION_KEY,
    meta::META_CAPI_TEST_EVENT_CODE_KEY,
    meta::META_CAPI_VERIFY_TOKEN_KEY,
    meta::GOOGLE_ADS_ENABLED_KEY,
    meta::GOOGLE_ADS_CUSTOMER_ID_KEY,
    meta::GOOGLE_ADS_DEVELOPER_TOKEN_KEY,
    meta::GOOGLE_ADS_REFRESH_TOKEN_KEY,
    meta::GOOGLE_ADS_CLIENT_ID_KEY,
    meta::GOOGLE_ADS_CLIENT_SECRET_KEY,
    meta::GOOGLE_ADS_CONVERSION_ACTION_ID_KEY,
    meta::BANDSINTOWN_CONVERSION_ENABLED_KEY,
    meta::BANDSINTOWN_API_TOKEN_KEY,
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
    /// Optional immediately preceding commerce credential accepted during rotation.
    pub previous_commerce_api_key_sha256: Option<[u8; 32]>,
    /// Narrow Control Plane credential accepted only by the AREA management namespace.
    pub control_plane_area_api_key_sha256: Option<[u8; 32]>,
    /// Optional immediately preceding AREA management credential accepted during rotation.
    pub previous_control_plane_area_api_key_sha256: Option<[u8; 32]>,
    /// Separate narrow credential for operational visibility and bounded controls.
    pub control_plane_api_key_sha256: Option<[u8; 32]>,
    /// Optional immediately preceding control-plane credential accepted during rotation.
    pub previous_control_plane_api_key_sha256: Option<[u8; 32]>,
    pub event_reminder_offsets_minutes: Vec<u32>,
    pub event_reminder_poll_interval: Duration,
    pub autopilot_enabled: bool,
    pub autopilot_poll_interval: Duration,
    /// When true, the agent outcome worker polls `agent_outcomes` and maps
    /// LLM-produced outcomes into autopilot decisions. Default ON.
    pub agent_outcomes_enabled: bool,
    pub admission_security: AdmissionSecurityConfig,
    /// Optional secret-backed team contacts used only to bootstrap routing identities.
    pub team_operations: TeamOperationsConfig,
    /// Derived AEAD key for sensitive idempotency response replay.
    pub response_encryption_key: SensitiveResponseKey,
    /// Optional immediately preceding AEAD key used during bounded rotation.
    pub previous_response_encryption_key: Option<SensitiveResponseKey>,
    /// Requires inbox ownership confirmation before a fan becomes active.
    pub require_double_opt_in: bool,
    /// Public/runtime push-delivery controls. Provider secrets remain worker-only.
    pub push_delivery: PushPublicConfig,
    /// Edge rate limiting policy applied by the HTTP layer.
    pub rate_limit: RateLimitConfig,
    /// Server-side ad conversion tracking (Meta CAPI, Google Ads, Bandsintown).
    pub ad_conversion: AdConversionConfig,
}

/// Per-identity fixed-window limits enforced at the API edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimitConfig {
    /// Master switch; when false the limiter passes every request.
    pub enabled: bool,
    /// Token issuance and redemption endpoints (magic links, pairing codes).
    pub public_auth_per_minute: u32,
    /// Privileged namespaces guarded by static bearer credentials.
    pub privileged_per_minute: u32,
    /// Everything else, as a coarse flood damper.
    pub general_per_minute: u32,
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
        let previous_commerce_api_key_sha256 =
            parse_commerce_api_key(values.get(PREVIOUS_COMMERCE_API_KEY))?;
        if previous_commerce_api_key_sha256.is_some()
            && previous_commerce_api_key_sha256 == commerce_api_key_sha256
        {
            return Err(ConfigError::InvalidSecret {
                name: PREVIOUS_COMMERCE_API_KEY,
            });
        }
        // config/parsing.rs is include!d into this module rather than being a
        // submodule, so its helpers are already in scope unqualified.
        let control_plane_area_api_key_sha256 = parse_optional_secret_hash(
            values.get(CONTROL_PLANE_AREA_API_KEY),
            CONTROL_PLANE_AREA_API_KEY,
        )?;
        let previous_control_plane_area_api_key_sha256 = parse_optional_secret_hash(
            values.get(PREVIOUS_CONTROL_PLANE_AREA_API_KEY),
            PREVIOUS_CONTROL_PLANE_AREA_API_KEY,
        )?;
        let control_plane_api_key_sha256 =
            parse_optional_secret_hash(values.get(CONTROL_PLANE_API_KEY), CONTROL_PLANE_API_KEY)?;
        let previous_control_plane_api_key_sha256 = parse_optional_secret_hash(
            values.get(PREVIOUS_CONTROL_PLANE_API_KEY),
            PREVIOUS_CONTROL_PLANE_API_KEY,
        )?;
        for (previous, current, name) in [
            (
                previous_control_plane_area_api_key_sha256,
                control_plane_area_api_key_sha256,
                PREVIOUS_CONTROL_PLANE_AREA_API_KEY,
            ),
            (
                previous_control_plane_api_key_sha256,
                control_plane_api_key_sha256,
                PREVIOUS_CONTROL_PLANE_API_KEY,
            ),
        ] {
            if previous.is_some() && previous == current {
                return Err(ConfigError::InvalidSecret { name });
            }
        }
        let event_reminder_offsets_minutes =
            parse_event_reminder_offsets(values.get(EVENT_REMINDER_OFFSETS_MINUTES_KEY))?;
        let event_reminder_poll_interval = parse_bounded_duration(
            values.get(EVENT_REMINDER_POLL_INTERVAL_MS_KEY),
            EVENT_REMINDER_POLL_INTERVAL_MS_KEY,
            DEFAULT_EVENT_REMINDER_POLL_INTERVAL_MS,
            MIN_EVENT_REMINDER_POLL_INTERVAL_MS,
            MAX_EVENT_REMINDER_POLL_INTERVAL_MS,
        )?;
        let autopilot_enabled = parse_bool(
            values.get(AUTOPILOT_ENABLED_KEY),
            AUTOPILOT_ENABLED_KEY,
            false,
        )?;
        let agent_outcomes_enabled = parse_bool(
            values.get(AGENT_OUTCOMES_ENABLED_KEY),
            AGENT_OUTCOMES_ENABLED_KEY,
            true,
        )?;
        let autopilot_poll_interval = parse_bounded_duration(
            values.get(AUTOPILOT_POLL_INTERVAL_MS_KEY),
            AUTOPILOT_POLL_INTERVAL_MS_KEY,
            DEFAULT_AUTOPILOT_POLL_INTERVAL_MS,
            MIN_AUTOPILOT_POLL_INTERVAL_MS,
            MAX_AUTOPILOT_POLL_INTERVAL_MS,
        )?;
        let admission_security = parse_admission_security(&values, environment.is_production())?;
        let team_operations = parse_team_operations(&values)?;
        validate_production_team_contacts(
            &team_operations,
            environment.is_production(),
            autopilot_enabled,
        )?;
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
        let rate_limit = parse_rate_limit(&values)?;
        let ad_conversion = AdConversionConfig::parse(&values)?;

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
            previous_commerce_api_key_sha256,
            control_plane_area_api_key_sha256,
            previous_control_plane_area_api_key_sha256,
            control_plane_api_key_sha256,
            previous_control_plane_api_key_sha256,
            event_reminder_offsets_minutes,
            event_reminder_poll_interval,
            autopilot_enabled,
            autopilot_poll_interval,
            agent_outcomes_enabled,
            admission_security,
            team_operations,
            response_encryption_key,
            previous_response_encryption_key,
            require_double_opt_in,
            push_delivery: PushPublicConfig::parse(&values)?,
            rate_limit,
            ad_conversion,
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
    /// Optional preceding admin credential accepted during bounded rotation.
    pub previous_admin_api_key_sha256: Option<[u8; 32]>,
    /// Optional preceding staff credential accepted during bounded rotation.
    pub previous_staff_api_key_sha256: Option<[u8; 32]>,
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
            .field(
                "previous_admin_api_key_sha256",
                &self.previous_admin_api_key_sha256.map(|_| "[REDACTED]"),
            )
            .field(
                "previous_staff_api_key_sha256",
                &self.previous_staff_api_key_sha256.map(|_| "[REDACTED]"),
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

    /// Production Autopilot needs every configured team owner to have a secret-backed contact.
    #[error("production Autopilot team routing requires {name}")]
    MissingProductionTeamContact { name: &'static str },

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

include!("config/parsing.rs");
include!("config/tests.rs");
