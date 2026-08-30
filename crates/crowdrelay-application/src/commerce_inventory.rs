//! Commerce inventory repository port.
//!
//! Moves all SQL write operations (stocktake, inventory activation) out of the
//! API layer. The API layer retains pure validation (normalize, hash, text
//! bounds) and response formatting; the adapter implementation owns every
//! INSERT/UPDATE against `inventory_activation_state`, `inventory_stocktakes`,
//! `inventory_stocktake_items`, `inventory_ledger`, and
//! `ecosystem_feature_flags`.

use async_trait::async_trait;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

/// Error returned by commerce inventory repository operations.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CommerceInventoryError {
    #[error("commerce inventory resource not found")]
    NotFound,
    #[error("commerce inventory request conflicts with existing state")]
    Conflict,
    #[error("commerce inventory request is invalid")]
    Invalid,
    #[error("commerce inventory repository is temporarily unavailable")]
    Unavailable,
}

/// A single variant target within a stocktake command.
#[derive(Clone, Debug)]
pub struct StocktakeItemInput {
    pub sku: String,
    pub on_hand: i32,
}

/// Command to perform an exact physical stocktake for a workspace.
#[derive(Clone, Debug)]
pub struct StocktakeCommand {
    pub workspace_id: Uuid,
    pub idempotency_key: String,
    pub request_hash: Vec<u8>,
    pub actor_id: Option<String>,
    pub reason: Option<String>,
    pub items: Vec<StocktakeItemInput>,
}

/// A single variant result row within a stocktake result.
#[derive(Clone, Debug)]
pub struct StocktakeItemResult {
    pub sku: String,
    pub label: String,
    pub target_on_hand: i32,
    pub on_hand_before: i64,
    pub reserved_at_apply: i64,
    pub applied_delta: i32,
    pub available_quantity: i64,
}

/// Result of performing a stocktake.
#[derive(Clone, Debug)]
pub struct StocktakeResult {
    pub id: Uuid,
    pub replayed: bool,
    pub created_at: OffsetDateTime,
    pub items: Vec<StocktakeItemResult>,
}

/// Snapshot of the inventory activation state for a workspace.
#[derive(Clone, Debug)]
pub struct InventoryActivationState {
    pub status: String,
    pub ready_at: Option<OffsetDateTime>,
    pub ready_by: Option<String>,
    pub version: i32,
}

/// Command to mark a workspace's inventory as ready for live writes.
#[derive(Clone, Debug)]
pub struct MarkInventoryReadyCommand {
    pub workspace_id: Uuid,
    pub actor_id: String,
    pub request_id: Option<String>,
}

/// Result of marking inventory ready.
#[derive(Clone, Debug)]
pub struct MarkInventoryReadyResult {
    pub activation: InventoryActivationState,
    pub enabled_feature_flags: Vec<String>,
}

/// Repository port for commerce inventory write operations.
///
/// Each method encapsulates a full transaction (reads + writes) so the
/// API handler is pure input validation and response formatting.
#[async_trait]
pub trait CommerceInventoryRepository: Send + Sync {
    /// Perform an exact physical stocktake. Idempotent by `idempotency_key`:
    /// a replay with a matching `request_hash` returns the original result,
    /// while a mismatched hash yields a [`CommerceInventoryError::Conflict`].
    async fn stocktake(
        &self,
        command: &StocktakeCommand,
    ) -> Result<StocktakeResult, CommerceInventoryError>;

    /// Mark a workspace's inventory as ready for live writes, enabling the
    /// merch inventory feature flags. Returns [`CommerceInventoryError::Conflict`]
    /// if the inventory is already ready or preconditions are not met.
    async fn mark_inventory_ready(
        &self,
        command: &MarkInventoryReadyCommand,
    ) -> Result<MarkInventoryReadyResult, CommerceInventoryError>;
}
