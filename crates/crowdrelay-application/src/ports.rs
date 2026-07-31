//! Ports implemented by infrastructure adapters.
//!
//! Defines the repository trait, idempotency key, request ID, and command
//! types used by the acquisition use cases. Infrastructure crates implement
//! these traits against PostgreSQL or other backends.

use std::fmt;

use async_trait::async_trait;
use crowdrelay_domain::{
    CitySignal, ClickEvent, FanSignup, FanSignupResult, ResolvedSmartLink, WorkspaceId,
    WorkspaceSlug,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

/// Idempotency key sent by the client to make write operations replayable.
///
/// `Debug` is deliberately redacted to prevent accidental logging.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// Parses an idempotency key, accepting 8–128 visible ASCII characters
    /// (excluding `"` and `\\`).
    pub fn parse(value: impl AsRef<str>) -> Result<Self, TextKeyError> {
        validate_text_key(value.as_ref(), 8, 128).map(Self)
    }

    /// Returns the key as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the value object and returns the underlying string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Debug for IdempotencyKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("IdempotencyKey")
            .field(&"[REDACTED]")
            .finish()
    }
}

impl Serialize for IdempotencyKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for IdempotencyKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(de::Error::custom)
    }
}

/// Request identifier propagated through logs, outbox events, and webhooks.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RequestId(String);

impl RequestId {
    /// Parses a request ID, accepting 1–128 visible ASCII characters.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, TextKeyError> {
        validate_text_key(value.as_ref(), 1, 128).map(Self)
    }

    /// Returns the request ID as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the value object and returns the underlying string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl<'de> Deserialize<'de> for RequestId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(de::Error::custom)
    }
}

fn validate_text_key(value: &str, min: usize, max: usize) -> Result<String, TextKeyError> {
    if !(min..=max).contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'"' && byte != b'\\')
    {
        return Err(TextKeyError { min, max });
    }
    Ok(value.to_owned())
}

/// Error returned when a text key fails validation.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("value must contain {min} to {max} safe visible ASCII characters")]
pub struct TextKeyError {
    min: usize,
    max: usize,
}

/// Command carrying a fan signup with idempotency and request tracing metadata.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct SignupFanCommand {
    idempotency_key: IdempotencyKey,
    request_id: RequestId,
    signup: FanSignup,
}

impl SignupFanCommand {
    /// Creates a signup command from validated components.
    #[must_use]
    pub const fn new(
        idempotency_key: IdempotencyKey,
        request_id: RequestId,
        signup: FanSignup,
    ) -> Self {
        Self {
            idempotency_key,
            request_id,
            signup,
        }
    }

    /// Returns the idempotency key.
    #[must_use]
    pub fn idempotency_key(&self) -> &IdempotencyKey {
        &self.idempotency_key
    }

    /// Returns the request ID for tracing.
    #[must_use]
    pub fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    /// Returns the fan signup payload.
    #[must_use]
    pub fn signup(&self) -> &FanSignup {
        &self.signup
    }

    /// Consumes the command and returns the inner signup.
    #[must_use]
    pub fn into_signup(self) -> FanSignup {
        self.signup
    }
}

impl fmt::Debug for SignupFanCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SignupFanCommand")
            .field("idempotency_key", &self.idempotency_key)
            .field("request_id", &self.request_id)
            .field("signup", &self.signup)
            .finish()
    }
}

/// Error returned by acquisition repository operations.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum RepositoryError {
    /// The repository is temporarily unavailable (e.g. connection lost).
    #[error("acquisition repository is temporarily unavailable")]
    Unavailable,
    /// The requested resource was not found.
    #[error("requested acquisition resource was not found")]
    NotFound,
    /// The request conflicts with existing state (e.g. duplicate signup).
    #[error("acquisition request conflicts with existing state")]
    Conflict,
    /// An unexpected error occurred in the repository.
    #[error("acquisition repository failed unexpectedly")]
    Unexpected,
}

/// Repository port for smart-link resolution, click persistence, fan signup,
/// and city signal listing.
#[async_trait]
pub trait AcquisitionRepository: Send + Sync {
    /// Resolves a workspace by its slug.
    async fn resolve_workspace(
        &self,
        slug: &WorkspaceSlug,
    ) -> Result<Option<WorkspaceId>, RepositoryError>;

    /// Loads active links visible to this adapter. A tenant-scoped adapter may
    /// return only its configured trusted workspace.
    async fn load_active_smart_links(&self) -> Result<Vec<ResolvedSmartLink>, RepositoryError>;

    /// Persists a best-effort analytics batch. Callers may drop clicks before
    /// this port under bounded-channel overload.
    async fn persist_click_batch(&self, clicks: &[ClickEvent]) -> Result<(), RepositoryError>;

    /// Atomically persists fan, consent, city interest, acquisition context,
    /// city aggregate, personal referral code, outbox event and idempotency
    /// response. Replays return the original result.
    async fn persist_fan_signup(
        &self,
        command: &SignupFanCommand,
    ) -> Result<FanSignupResult, RepositoryError>;

    /// Lists city signals for the given workspace, sorted by fan count.
    async fn list_city_signals(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
    ) -> Result<Vec<CitySignal>, RepositoryError>;
}
