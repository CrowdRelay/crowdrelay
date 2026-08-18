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
    /// The addressed resource does not exist in this workspace.
    #[error("ecosystem resource was not found")]
    NotFound,
    /// The idempotency key was reused with an incompatible payload.
    #[error("ecosystem mutation conflicts with a previous request")]
    Conflict,
    /// The repository failed for a reason the caller cannot act on.
    #[error("ecosystem repository failed unexpectedly")]
    Unexpected,
}

/// One row of a show's run-of-day checklist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowChecklistItemState {
    pub item_key: String,
    pub status: String,
    pub note: Option<String>,
    pub updated_at: OffsetDateTime,
}

/// An operator request to set one checklist item's status.
///
/// The event is addressed by slug because that is what the operator surface
/// knows; resolving it to an id is the repository's job.
#[derive(Debug, Clone)]
pub struct UpdateShowChecklistCommand {
    pub workspace_id: WorkspaceId,
    pub event_slug: String,
    pub item_key: String,
    pub status: String,
    pub note: Option<String>,
    pub idempotency_key: String,
    pub request_id: Option<String>,
}

/// Outcome of a checklist update: the event it belongs to, every item after the
/// write, and whether the replay window served it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowChecklistMutation {
    pub event_id: uuid::Uuid,
    pub items: Vec<ShowChecklistItemState>,
    pub replayed: bool,
}

/// A completed reconciliation pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciliationRunState {
    pub id: uuid::Uuid,
    pub status: String,
    pub trigger: String,
    pub finding_count: i32,
    pub started_at: OffsetDateTime,
    pub finished_at: Option<OffsetDateTime>,
}

/// One discrepancy the pass found between two authoritative records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciliationFindingState {
    pub id: uuid::Uuid,
    pub run_id: uuid::Uuid,
    pub kind: String,
    pub severity: String,
    pub entity_type: String,
    pub entity_id: Option<uuid::Uuid>,
    pub entity_label: Option<String>,
    pub summary: String,
    pub suggested_action: Option<String>,
    pub metadata: serde_json::Value,
    pub created_at: OffsetDateTime,
    pub resolved_at: Option<OffsetDateTime>,
}

/// An operator request to run reconciliation.
#[derive(Debug, Clone)]
pub struct RunReconciliationCommand {
    pub workspace_id: WorkspaceId,
    pub trigger: String,
    pub idempotency_key: String,
    pub request_id: Option<String>,
}

/// The run and everything it raised, plus whether the replay window served it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciliationOutcome {
    pub run: ReconciliationRunState,
    pub findings: Vec<ReconciliationFindingState>,
    pub replayed: bool,
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

    /// Sets one checklist item and records the operator action in the same
    /// transaction, under the same replay rules as a flag flip.
    ///
    /// `NotFound` means the event slug does not resolve in this workspace. As
    /// with flags, the caller owns input policy: which statuses and item keys
    /// are legal is validated before the command is built.
    async fn update_show_checklist(
        &self,
        command: &UpdateShowChecklistCommand,
    ) -> Result<ShowChecklistMutation, EcosystemRepositoryError>;

    /// Runs one reconciliation pass: raises findings, closes the run, emits an
    /// outbox event per actionable finding and records the operator action, all
    /// in one transaction.
    ///
    /// A replay returns the original run rather than starting a second pass, so
    /// a retried request cannot double-count findings or re-emit their events.
    /// The caller validates the trigger vocabulary.
    async fn run_reconciliation(
        &self,
        command: &RunReconciliationCommand,
    ) -> Result<ReconciliationOutcome, EcosystemRepositoryError>;
}
