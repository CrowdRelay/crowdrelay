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

const MAX_PANIC_MESSAGE_CHARS: usize = 1_024;

/// Installs a bounded structured panic report for the current process.
///
/// The hook deliberately omits backtraces and arbitrary debug payloads so a
/// panic cannot copy secrets or an unbounded value into production logs.
pub fn install_panic_hook(service_name: &'static str) {
    std::panic::set_hook(Box::new(move |panic_info| {
        let message = panic_message(panic_info);
        let location = panic_info.location();
        let file = location
            .map(std::panic::Location::file)
            .unwrap_or("unknown");
        let line = location.map(std::panic::Location::line).unwrap_or(0);
        let column = location.map(std::panic::Location::column).unwrap_or(0);
        tracing::error!(
            service.name = service_name,
            panic.message = %message,
            panic.file = file,
            panic.line = line,
            panic.column = column,
            "process panic"
        );
    }));
}

fn panic_message(panic_info: &std::panic::PanicHookInfo<'_>) -> String {
    let raw = panic_info
        .payload()
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| {
            panic_info
                .payload()
                .downcast_ref::<String>()
                .map(String::as_str)
        })
        .unwrap_or("non-string panic payload");
    raw.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(MAX_PANIC_MESSAGE_CHARS)
        .collect()
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
