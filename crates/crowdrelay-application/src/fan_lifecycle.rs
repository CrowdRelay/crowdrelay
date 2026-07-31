//! Fan confirmation and unsubscribe application ports.

use std::sync::Arc;

use async_trait::async_trait;
use crowdrelay_domain::{FanActionToken, FanConfirmationResult, FanUnsubscribeResult, WorkspaceId};
use thiserror::Error;

use crate::{IdempotencyKey, RepositoryError, RequestId};

/// Idempotent exchange of a one-time inbox confirmation token for a fan session.
#[derive(Clone, Debug)]
pub struct ConfirmFanCommand {
    pub workspace_id: WorkspaceId,
    pub token: FanActionToken,
    pub idempotency_key: IdempotencyKey,
    pub request_id: RequestId,
}

/// Persistence boundary for one-time fan lifecycle actions.
#[async_trait]
pub trait FanLifecycleRepository: Send + Sync {
    async fn confirm(
        &self,
        command: &ConfirmFanCommand,
    ) -> Result<FanConfirmationResult, RepositoryError>;

    async fn unsubscribe(
        &self,
        workspace_id: WorkspaceId,
        token: &FanActionToken,
    ) -> Result<FanUnsubscribeResult, RepositoryError>;
}

/// Confirms inbox ownership and activates a pending fan.
#[derive(Clone)]
pub struct ConfirmFan {
    repository: Arc<dyn FanLifecycleRepository>,
}

impl ConfirmFan {
    /// Creates a confirmation use case.
    #[must_use]
    pub fn new(repository: Arc<dyn FanLifecycleRepository>) -> Self {
        Self { repository }
    }

    /// Consumes a one-time confirmation token.
    pub async fn execute(
        &self,
        command: &ConfirmFanCommand,
    ) -> Result<FanConfirmationResult, FanLifecycleError> {
        self.repository
            .confirm(command)
            .await
            .map_err(FanLifecycleError::Repository)
    }
}

/// Unsubscribes a fan and revokes active browser sessions.
#[derive(Clone)]
pub struct UnsubscribeFan {
    repository: Arc<dyn FanLifecycleRepository>,
}

impl UnsubscribeFan {
    /// Creates an unsubscribe use case.
    #[must_use]
    pub fn new(repository: Arc<dyn FanLifecycleRepository>) -> Self {
        Self { repository }
    }

    /// Consumes a one-time unsubscribe token.
    pub async fn execute(
        &self,
        workspace_id: WorkspaceId,
        token: &FanActionToken,
    ) -> Result<FanUnsubscribeResult, FanLifecycleError> {
        self.repository
            .unsubscribe(workspace_id, token)
            .await
            .map_err(FanLifecycleError::Repository)
    }
}

/// Error returned by fan lifecycle operations.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum FanLifecycleError {
    /// The repository returned an error.
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}
