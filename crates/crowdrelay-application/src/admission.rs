//! Admission-pass application ports and use cases.

use std::sync::Arc;

use async_trait::async_trait;
use crowdrelay_domain::{
    AdmissionPassClaimed, AdmissionPassId, AdmissionPassIssued, AdmissionPassView,
    AdmissionRedemptionResult, EventId, EventSlug, NormalizedEmail, PassClaimToken,
    PassSessionToken, WorkspaceId,
};
use thiserror::Error;

use crate::{IdempotencyKey, RepositoryError, RequestId};

/// Command used by an authenticated operator to issue one limited-pool pass.
#[derive(Clone, Debug)]
pub struct IssueAdmissionPassCommand {
    pub workspace_id: WorkspaceId,
    pub event_slug: EventSlug,
    pub pool_slug: EventSlug,
    pub fan_email: NormalizedEmail,
    pub claim_expires_hours: u32,
    pub idempotency_key: IdempotencyKey,
    pub request_id: RequestId,
}

/// Idempotent exchange of a one-time winner token for a private session.
#[derive(Clone, Debug)]
pub struct ClaimAdmissionPassCommand {
    pub workspace_id: WorkspaceId,
    pub token: PassClaimToken,
    pub idempotency_key: IdempotencyKey,
    pub request_id: RequestId,
}

/// Command used by gate staff after QR verification or manual reference lookup.
#[derive(Clone, Debug)]
pub struct RedeemAdmissionPassCommand {
    pub workspace_id: WorkspaceId,
    pub event_slug: EventSlug,
    pub pass_id: Option<AdmissionPassId>,
    pub event_id: Option<EventId>,
    pub public_reference: String,
    pub idempotency_key: IdempotencyKey,
    pub request_id: RequestId,
}

/// Command used by an operator to revoke an unused pass.
#[derive(Clone, Debug)]
pub struct RevokeAdmissionPassCommand {
    pub workspace_id: WorkspaceId,
    pub public_reference: String,
    pub idempotency_key: IdempotencyKey,
    pub request_id: RequestId,
}

/// Persistence boundary for admission issuance, claim, lookup, and redemption.
#[async_trait]
pub trait AdmissionRepository: Send + Sync {
    async fn issue_pass(
        &self,
        command: &IssueAdmissionPassCommand,
    ) -> Result<AdmissionPassIssued, RepositoryError>;

    async fn claim_pass(
        &self,
        command: &ClaimAdmissionPassCommand,
    ) -> Result<AdmissionPassClaimed, RepositoryError>;

    async fn load_pass(
        &self,
        workspace_id: WorkspaceId,
        session: &PassSessionToken,
    ) -> Result<AdmissionPassView, RepositoryError>;

    async fn redeem_pass(
        &self,
        command: &RedeemAdmissionPassCommand,
    ) -> Result<AdmissionRedemptionResult, RepositoryError>;

    async fn revoke_pass(
        &self,
        command: &RevokeAdmissionPassCommand,
    ) -> Result<AdmissionPassView, RepositoryError>;
}

/// Issues a pass while enforcing the configured limited-pool capacity.
#[derive(Clone)]
pub struct IssueAdmissionPass {
    repository: Arc<dyn AdmissionRepository>,
}

impl IssueAdmissionPass {
    /// Creates an issuance use case backed by the supplied repository.
    #[must_use]
    pub fn new(repository: Arc<dyn AdmissionRepository>) -> Self {
        Self { repository }
    }

    /// Issues or idempotently replays an admission pass.
    pub async fn execute(
        &self,
        command: &IssueAdmissionPassCommand,
    ) -> Result<AdmissionPassIssued, AdmissionUseCaseError> {
        validate_reference_slug(command.pool_slug.as_str())?;
        if !(1..=720).contains(&command.claim_expires_hours) {
            return Err(AdmissionUseCaseError::InvalidInput);
        }
        self.repository
            .issue_pass(command)
            .await
            .map_err(AdmissionUseCaseError::Repository)
    }
}

/// Exchanges a one-time winner token for a private pass session.
#[derive(Clone)]
pub struct ClaimAdmissionPass {
    repository: Arc<dyn AdmissionRepository>,
}

impl ClaimAdmissionPass {
    /// Creates a pass-claim use case.
    #[must_use]
    pub fn new(repository: Arc<dyn AdmissionRepository>) -> Self {
        Self { repository }
    }

    /// Consumes the claim token and returns the initial private pass view.
    pub async fn execute(
        &self,
        command: &ClaimAdmissionPassCommand,
    ) -> Result<AdmissionPassClaimed, AdmissionUseCaseError> {
        self.repository
            .claim_pass(command)
            .await
            .map_err(AdmissionUseCaseError::Repository)
    }
}

/// Loads a pass belonging to the current private winner session.
#[derive(Clone)]
pub struct LoadAdmissionPass {
    repository: Arc<dyn AdmissionRepository>,
}

impl LoadAdmissionPass {
    /// Creates a pass lookup use case.
    #[must_use]
    pub fn new(repository: Arc<dyn AdmissionRepository>) -> Self {
        Self { repository }
    }

    /// Returns the current pass view and refreshes session activity.
    pub async fn execute(
        &self,
        workspace_id: WorkspaceId,
        session: &PassSessionToken,
    ) -> Result<AdmissionPassView, AdmissionUseCaseError> {
        self.repository
            .load_pass(workspace_id, session)
            .await
            .map_err(AdmissionUseCaseError::Repository)
    }
}

/// Atomically redeems a pass exactly once at the venue gate.
#[derive(Clone)]
pub struct RedeemAdmissionPass {
    repository: Arc<dyn AdmissionRepository>,
}

impl RedeemAdmissionPass {
    /// Creates a pass-redemption use case.
    #[must_use]
    pub fn new(repository: Arc<dyn AdmissionRepository>) -> Self {
        Self { repository }
    }

    /// Redeems by verified QR claims or by a manually entered public reference.
    pub async fn execute(
        &self,
        command: &RedeemAdmissionPassCommand,
    ) -> Result<AdmissionRedemptionResult, AdmissionUseCaseError> {
        validate_public_reference(&command.public_reference)?;
        if command.pass_id.is_some() != command.event_id.is_some() {
            return Err(AdmissionUseCaseError::InvalidInput);
        }
        self.repository
            .redeem_pass(command)
            .await
            .map_err(AdmissionUseCaseError::Repository)
    }
}

/// Revokes a pass before admission.
#[derive(Clone)]
pub struct RevokeAdmissionPass {
    repository: Arc<dyn AdmissionRepository>,
}

impl RevokeAdmissionPass {
    /// Creates a pass-revocation use case.
    #[must_use]
    pub fn new(repository: Arc<dyn AdmissionRepository>) -> Self {
        Self { repository }
    }

    /// Revokes or idempotently returns an already revoked pass.
    pub async fn execute(
        &self,
        command: &RevokeAdmissionPassCommand,
    ) -> Result<AdmissionPassView, AdmissionUseCaseError> {
        validate_public_reference(&command.public_reference)?;
        self.repository
            .revoke_pass(command)
            .await
            .map_err(AdmissionUseCaseError::Repository)
    }
}

fn validate_reference_slug(value: &str) -> Result<(), AdmissionUseCaseError> {
    if value.is_empty() || value.len() > 128 {
        return Err(AdmissionUseCaseError::InvalidInput);
    }
    Ok(())
}

fn validate_public_reference(value: &str) -> Result<(), AdmissionUseCaseError> {
    if !(8..=64).contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(AdmissionUseCaseError::InvalidInput);
    }
    Ok(())
}

/// Error returned by admission use cases.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum AdmissionUseCaseError {
    /// The admission request failed input validation.
    #[error("admission request is invalid")]
    InvalidInput,
    /// The repository returned an error.
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_reference_validation_matches_gate_contract() {
        assert!(validate_public_reference("VIRYA-ABC12345").is_ok());
        assert!(validate_public_reference("short").is_err());
        assert!(validate_public_reference("VIRYA/INVALID").is_err());
    }
}
