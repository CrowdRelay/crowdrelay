use std::{collections::HashMap, fmt, sync::Arc};

use async_trait::async_trait;
use thiserror::Error;

const MINIMUM_SECRET_BYTES: usize = 32;
const MAXIMUM_SECRET_BYTES: usize = 4096;

/// Opaque HMAC key material.
#[derive(Clone)]
pub struct SecretValue(Arc<[u8]>);

impl SecretValue {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, SecretValueError> {
        let bytes = bytes.into();
        if !(MINIMUM_SECRET_BYTES..=MAXIMUM_SECRET_BYTES).contains(&bytes.len()) {
            return Err(SecretValueError::InvalidLength {
                minimum: MINIMUM_SECRET_BYTES,
                maximum: MAXIMUM_SECRET_BYTES,
            });
        }

        Ok(Self(Arc::from(bytes)))
    }

    pub(super) fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretValue([REDACTED])")
    }
}

/// Error returned when a webhook secret value fails validation.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SecretValueError {
    /// The secret length was outside the allowed range.
    #[error("webhook secret must contain between {minimum} and {maximum} bytes")]
    InvalidLength { minimum: usize, maximum: usize },
}

/// Classification of secret provider failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretProviderErrorKind {
    /// The backing secret service is temporarily unavailable.
    Unavailable,
    /// The configured reference does not exist and needs operator action.
    NotFound,
    /// The referenced value is unsuitable for signing.
    Invalid,
}

/// Sanitized provider failure. It intentionally carries no secret value or
/// provider response body.
/// Error returned when a secret provider fails to resolve a reference.
#[derive(Debug, Error)]
#[error("webhook secret resolution failed ({kind:?})")]
pub struct SecretProviderError {
    kind: SecretProviderErrorKind,
}

impl SecretProviderError {
    pub const fn new(kind: SecretProviderErrorKind) -> Self {
        Self { kind }
    }

    pub const fn kind(&self) -> SecretProviderErrorKind {
        self.kind
    }
}

/// Explicit port for resolving a configured secret reference.
///
/// Environment, OCI Vault, Docker secrets, or another backend belong in an
/// adapter supplied by the binary. The outbox module never reads arbitrary
/// environment variables itself.
#[async_trait]
pub trait SecretProvider: Send + Sync + 'static {
    async fn resolve(&self, reference: &str) -> Result<SecretValue, SecretProviderError>;
}

/// Immutable provider useful for Docker secrets loaded by the composition
/// root and for tests. References are safe to log; values are always redacted.
#[derive(Clone, Default)]
pub struct MapSecretProvider {
    secrets: Arc<HashMap<String, SecretValue>>,
}

impl MapSecretProvider {
    pub fn new(secrets: HashMap<String, SecretValue>) -> Self {
        Self {
            secrets: Arc::new(secrets),
        }
    }
}

impl fmt::Debug for MapSecretProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MapSecretProvider")
            .field("secret_count", &self.secrets.len())
            .finish()
    }
}

#[async_trait]
impl SecretProvider for MapSecretProvider {
    async fn resolve(&self, reference: &str) -> Result<SecretValue, SecretProviderError> {
        self.secrets
            .get(reference)
            .cloned()
            .ok_or_else(|| SecretProviderError::new(SecretProviderErrorKind::NotFound))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_contains_secret() -> Result<(), Box<dyn std::error::Error>> {
        let value = SecretValue::new(b"0123456789abcdef0123456789abcdef".to_vec())?;

        let rendered = format!("{value:?}");
        assert_eq!(rendered, "SecretValue([REDACTED])");
        assert!(!rendered.contains("0123456789abcdef"));
        Ok(())
    }

    #[tokio::test]
    async fn map_provider_distinguishes_missing_references() {
        let provider = MapSecretProvider::default();
        let error = provider
            .resolve("missing")
            .await
            .expect_err("missing reference must fail");

        assert_eq!(error.kind(), SecretProviderErrorKind::NotFound);
    }
}
