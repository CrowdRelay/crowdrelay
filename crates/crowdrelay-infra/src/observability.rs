//! Process-wide structured tracing.
//!
//! Installs a JSON-formatted `tracing` subscriber that honors `RUST_LOG`
//! for filter configuration.

use std::env;

use thiserror::Error;
use tracing_subscriber::{
    EnvFilter,
    filter::ParseError,
    util::{SubscriberInitExt, TryInitError},
};

const DEFAULT_FILTER: &str = "info";

/// Installs a process-wide JSON tracing subscriber.
///
/// `RUST_LOG` is honored when present and rejected when malformed. Calling this
/// more than once returns an error instead of replacing another subscriber.
pub fn init(service_name: &str) -> Result<(), ObservabilityError> {
    let service_name = service_name.trim();
    if service_name.is_empty() {
        return Err(ObservabilityError::EmptyServiceName);
    }

    let filter = filter_from_env()?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .json()
        .with_ansi(false)
        .with_current_span(true)
        .with_span_list(true)
        .finish()
        .try_init()
        .map_err(ObservabilityError::SetGlobalSubscriber)?;

    tracing::info!(service.name = service_name, "tracing initialized");
    Ok(())
}

fn filter_from_env() -> Result<EnvFilter, ObservabilityError> {
    match env::var("RUST_LOG") {
        Ok(value) => EnvFilter::try_new(value).map_err(ObservabilityError::InvalidFilter),
        Err(env::VarError::NotPresent) => Ok(EnvFilter::new(DEFAULT_FILTER)),
        Err(env::VarError::NotUnicode(_)) => Err(ObservabilityError::NonUnicodeFilter),
    }
}

/// Error returned when tracing subscriber initialization fails.
#[derive(Debug, Error)]
pub enum ObservabilityError {
    /// The service name was empty.
    #[error("service name must not be empty")]
    EmptyServiceName,

    /// `RUST_LOG` contained non-UTF-8 data.
    #[error("RUST_LOG is not valid Unicode")]
    NonUnicodeFilter,

    /// `RUST_LOG` contained an invalid filter expression.
    #[error("RUST_LOG contains an invalid tracing filter")]
    InvalidFilter(#[source] ParseError),

    /// A global tracing subscriber was already installed.
    #[error("global tracing subscriber is already initialized")]
    SetGlobalSubscriber(#[source] TryInitError),
}
