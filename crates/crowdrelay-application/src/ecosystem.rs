//! Ecosystem control-plane ports.
//!
//! Feature flags are an operator-facing kill-switch surface: the mutation is
//! idempotent, replay-validated and audited. `docs/ARCHITECTURE.md` puts that
//! kind of multi-row invariant behind a repository rather than in the HTTP
//! layer, so the contract lives here and the SQL lives in infrastructure.

use async_trait::async_trait;
use crowdrelay_domain::WorkspaceId;
use time::OffsetDateTime;

/// A single operator-visible feature flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureFlagState {
    pub key: String,
    pub enabled: bool,
    pub reason: Option<String>,
    pub version: i64,
    pub updated_at: OffsetDateTime,
}

/// An operator request to flip one flag.
///
/// `idempotency_key` scopes the replay window. Re-sending the same key with a
/// payload that hashes identically returns the stored outcome; re-sending it
/// with a different payload is a conflict rather than a silent overwrite.
#[derive(Debug, Clone)]
pub struct UpdateFeatureFlagCommand {
    pub workspace_id: WorkspaceId,
    pub key: String,
    pub enabled: bool,
    pub reason: Option<String>,
    pub idempotency_key: String,
    pub request_id: Option<String>,
}

/// Outcome of an update, including whether it was served from the replay window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureFlagMutation {
    pub flag: FeatureFlagState,
    pub replayed: bool,
}

/// Failure modes the HTTP layer maps onto status codes.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EcosystemRepositoryError {
    /// The flag key is not part of the declared flag set.
    #[error("unknown ecosystem feature flag")]
    UnknownFlag,
    /// The idempotency key was reused with an incompatible payload.
    #[error("ecosystem mutation conflicts with a previous request")]
    Conflict,
    /// The repository failed for a reason the caller cannot act on.
    #[error("ecosystem repository failed unexpectedly")]
    Unexpected,
}

/// Persistence boundary for the ecosystem control plane.
#[async_trait]
pub trait EcosystemControlPlaneRepository: Send + Sync {
    /// Applies a flag update inside one transaction that also records the
    /// operator action, so an accepted mutation is always auditable and a
    /// replayed one never writes twice.
    ///
    /// The caller owns the declared-flag set and must reject unknown keys:
    /// this upsert has to stay able to materialize a declared flag on its
    /// first flip, so it cannot tell an unknown key from an unmaterialized
    /// one. `UnknownFlag` therefore reports a row that vanished mid-write,
    /// not an undeclared key.
    async fn update_feature_flag(
        &self,
        command: &UpdateFeatureFlagCommand,
    ) -> Result<FeatureFlagMutation, EcosystemRepositoryError>;
}
