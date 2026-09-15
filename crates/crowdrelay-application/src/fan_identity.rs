//! Use cases and repository port for the fan identity spine (§4e-5): verified
//! identifiers, merge candidates, and the explicit reversible merge.

use std::sync::Arc;

use async_trait::async_trait;
use crowdrelay_domain::{
    WorkspaceId,
    fan_identity::{CandidateStatus, MergeError},
};
use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

/// A merge candidate as staff sees it — two fans plus the evidence that made
/// the system suspect they are one person.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeCandidateView {
    pub id: Uuid,
    pub fan_id_a: Uuid,
    pub fan_id_b: Uuid,
    pub evidence_kind: String,
    pub evidence: serde_json::Value,
    pub created_at: String,
}

/// The audit view of a completed merge — enough to answer "what moved" and
/// to drive the reversal.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FanMergeView {
    pub id: Uuid,
    pub survivor_fan_id: Uuid,
    pub merged_fan_id: Uuid,
    pub moved_counts: serde_json::Value,
    pub retained_counts: serde_json::Value,
    pub consents_mirrored: i32,
    pub merged_at: String,
    pub unmerged_at: Option<String>,
}

/// A fan's verified identifiers, for the merge review screen.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FanIdentifierView {
    pub kind: String,
    pub value: String,
    pub source: String,
    pub verified_at: String,
}

/// Command: execute an explicit merge of `merged_fan_id` into `survivor_fan_id`.
pub struct MergeFansCommand {
    pub workspace_id: WorkspaceId,
    pub survivor_fan_id: Uuid,
    pub merged_fan_id: Uuid,
    pub reason: Option<String>,
    pub merged_by: String,
    pub request_id: String,
}

/// Command: reverse the most recent unreversed merge of `merged_fan_id`.
pub struct UnmergeFanCommand {
    pub workspace_id: WorkspaceId,
    pub merged_fan_id: Uuid,
    pub unmerged_by: String,
    pub request_id: String,
}

/// Command: dismiss a pending candidate — a human decided these are two people.
pub struct DismissMergeCandidateCommand {
    pub workspace_id: WorkspaceId,
    pub candidate_id: Uuid,
}

/// Errors the use cases surface; each maps to one HTTP problem.
#[derive(Debug, Error)]
pub enum FanIdentityError {
    /// A referenced fan or candidate does not exist in this workspace.
    #[error("not found")]
    NotFound,
    /// The merge validation refused the request.
    #[error("merge refused: {0}")]
    InvalidMerge(#[from] MergeError),
    /// No reversible merge exists to undo.
    #[error("no open merge to reverse")]
    NothingToUnmerge,
    /// The candidate is already resolved.
    #[error("candidate already resolved")]
    AlreadyResolved,
    /// Store fault.
    #[error("fan identity repository failed unexpectedly")]
    Unavailable,
}

/// Repository port for identity-spine reads and the merge/unmerge transactions.
#[async_trait]
pub trait FanIdentityRepository: Send + Sync {
    /// Lists merge candidates for the workspace, newest first. `None` means
    /// pending only.
    async fn list_merge_candidates(
        &self,
        workspace_id: WorkspaceId,
        status: Option<CandidateStatus>,
        limit: u32,
    ) -> Result<Vec<MergeCandidateView>, FanIdentityError>;

    /// Lists a fan's verified identifiers.
    async fn list_fan_identifiers(
        &self,
        workspace_id: WorkspaceId,
        fan_id: Uuid,
    ) -> Result<Vec<FanIdentifierView>, FanIdentityError>;

    /// Executes the merge transaction: re-points movable rows, mirrors the
    /// latest consent state onto the survivor, tombstones the merged fan,
    /// writes the `fan_merges` audit row and resolves open candidates for the
    /// pair. Returns the audit view.
    async fn merge_fans(
        &self,
        command: &MergeFansCommand,
    ) -> Result<FanMergeView, FanIdentityError>;

    /// Reverses the latest unreversed merge for `merged_fan_id`: re-points
    /// the recorded `moved` rows back and restores the fan's prior status.
    async fn unmerge_fan(
        &self,
        command: &UnmergeFanCommand,
    ) -> Result<FanMergeView, FanIdentityError>;

    /// Marks a pending candidate dismissed. Already-resolved candidates error.
    async fn dismiss_merge_candidate(
        &self,
        command: &DismissMergeCandidateCommand,
    ) -> Result<(), FanIdentityError>;

    /// Lists merges for a fan (either direction), newest first.
    async fn list_fan_merges(
        &self,
        workspace_id: WorkspaceId,
        fan_id: Uuid,
    ) -> Result<Vec<FanMergeView>, FanIdentityError>;
}

/// Use cases over the identity repository.
pub struct FanIdentity {
    repository: Arc<dyn FanIdentityRepository>,
}

impl FanIdentity {
    /// Creates the use-case bundle.
    #[must_use]
    pub fn new(repository: Arc<dyn FanIdentityRepository>) -> Self {
        Self { repository }
    }

    /// Lists merge candidates (pending unless another status is asked for).
    pub async fn list_merge_candidates(
        &self,
        workspace_id: WorkspaceId,
        status: Option<CandidateStatus>,
        limit: u32,
    ) -> Result<Vec<MergeCandidateView>, FanIdentityError> {
        self.repository
            .list_merge_candidates(workspace_id, status, limit)
            .await
    }

    /// Lists a fan's verified identifiers.
    pub async fn list_fan_identifiers(
        &self,
        workspace_id: WorkspaceId,
        fan_id: Uuid,
    ) -> Result<Vec<FanIdentifierView>, FanIdentityError> {
        self.repository
            .list_fan_identifiers(workspace_id, fan_id)
            .await
    }

    /// Executes an explicit merge.
    pub async fn merge_fans(
        &self,
        command: &MergeFansCommand,
    ) -> Result<FanMergeView, FanIdentityError> {
        self.repository.merge_fans(command).await
    }

    /// Reverses the latest unreversed merge for a fan.
    pub async fn unmerge_fan(
        &self,
        command: &UnmergeFanCommand,
    ) -> Result<FanMergeView, FanIdentityError> {
        self.repository.unmerge_fan(command).await
    }

    /// Dismisses a pending candidate.
    pub async fn dismiss_merge_candidate(
        &self,
        command: &DismissMergeCandidateCommand,
    ) -> Result<(), FanIdentityError> {
        self.repository.dismiss_merge_candidate(command).await
    }

    /// Lists merges a fan took part in.
    pub async fn list_fan_merges(
        &self,
        workspace_id: WorkspaceId,
        fan_id: Uuid,
    ) -> Result<Vec<FanMergeView>, FanIdentityError> {
        self.repository.list_fan_merges(workspace_id, fan_id).await
    }
}

/// The evidence kind carried on a candidate row, re-exported for handlers.
pub use crowdrelay_domain::fan_identity::CandidateEvidenceKind as EvidenceKind;
