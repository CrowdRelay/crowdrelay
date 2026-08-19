//! Regional runtime defaults shared by the API and worker.
//!
//! CrowdRelay deployments are tenant-isolated, so a process-level default is
//! the correct fallback for regional behaviour that has not yet been overridden
//! by an end-user preference. PostgreSQL is the source of truth for supported
//! IANA timezone names because the worker evaluates quiet hours in SQL.

use std::env;

use sqlx::PgPool;
use thiserror::Error;

pub const DEFAULT_TIMEZONE_ENV: &str = "CROWDRELAY_DEFAULT_TIMEZONE";
pub const VIRYA_DEFAULT_TIMEZONE: &str = "Europe/Warsaw";
const MAX_TIMEZONE_LEN: usize = 64;

#[derive(Debug, Error)]
pub enum RegionalConfigError {
    #[error("{DEFAULT_TIMEZONE_ENV} must be a bounded IANA timezone name")]
    InvalidTimezoneSyntax,
    #[error("{DEFAULT_TIMEZONE_ENV} is not recognized by PostgreSQL: {timezone}")]
    UnknownTimezone { timezone: String },
    #[error("failed to validate {DEFAULT_TIMEZONE_ENV} against PostgreSQL")]
    TimezoneValidation(#[source] sqlx::Error),
}

/// Loads the tenant-wide timezone fallback without contacting external systems.
///
/// The syntax check is intentionally conservative. Exact IANA validity is
/// verified once against `pg_timezone_names` before either runtime starts.
pub fn default_timezone_from_env() -> Result<String, RegionalConfigError> {
    let value = env::var(DEFAULT_TIMEZONE_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| VIRYA_DEFAULT_TIMEZONE.to_owned());
    let value = value.trim();
    if !valid_timezone_syntax(value) {
        return Err(RegionalConfigError::InvalidTimezoneSyntax);
    }
    Ok(value.to_owned())
}

/// Fails startup if PostgreSQL cannot resolve the configured timezone.
pub async fn validate_timezone(
    database: &PgPool,
    timezone: &str,
) -> Result<(), RegionalConfigError> {
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM pg_timezone_names WHERE name = $1)",
    )
    .bind(timezone)
    .fetch_one(database)
    .await
    .map_err(RegionalConfigError::TimezoneValidation)?;
    if !exists {
        return Err(RegionalConfigError::UnknownTimezone {
            timezone: timezone.to_owned(),
        });
    }
    Ok(())
}

fn valid_timezone_syntax(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TIMEZONE_LEN
        && !value.starts_with('/')
        && !value.ends_with('/')
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-' | b'+')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timezone_syntax_accepts_portable_iana_names() {
        assert!(valid_timezone_syntax("Europe/Warsaw"));
        assert!(valid_timezone_syntax("Europe/Berlin"));
        assert!(valid_timezone_syntax("America/New_York"));
        assert!(!valid_timezone_syntax("America/New York"));
        assert!(!valid_timezone_syntax("/Europe/Warsaw"));
        assert!(!valid_timezone_syntax("Europe/Warsaw/"));
    }
}
