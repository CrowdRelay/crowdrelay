//! Beacon release admin repository port.
//!
//! Moves SQL write operations from the API layer's
//! `beacon_signal/releases/admin.rs` into infrastructure. The API handlers
//! call this port; the Postgres adapter executes the full transaction.

use async_trait::async_trait;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

/// Error returned by beacon release admin repository operations.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum BeaconReleaseAdminError {
    #[error("beacon release resource not found")]
    NotFound,
    #[error("beacon release request conflicts with existing state")]
    Conflict,
    #[error("beacon release request is invalid")]
    Invalid,
    #[error("beacon release request is malformed")]
    BadRequest,
    #[error("beacon release repository is temporarily unavailable")]
    Unavailable,
}

/// Command to create a release campaign.
#[derive(Clone, Debug)]
pub struct CreateReleaseCampaignCommand {
    pub workspace_id: Uuid,
    pub idempotency_key: String,
    pub request_id: Option<String>,
    pub slug: String,
    pub title: String,
    pub sku: String,
    pub claim_deadline: OffsetDateTime,
}

/// Result of creating a release campaign.
#[derive(Clone, Debug)]
pub struct CreateReleaseCampaignResult {
    pub campaign_id: Uuid,
    pub replayed: bool,
}

/// Command to launch a release campaign.
#[derive(Clone, Debug)]
pub struct LaunchReleaseCampaignCommand {
    pub workspace_id: Uuid,
    pub campaign_id: Uuid,
    pub idempotency_key: String,
    pub request_id: Option<String>,
}

/// Result of launching a release campaign.
#[derive(Clone, Debug)]
pub struct LaunchReleaseCampaignResult {
    pub replayed: bool,
    pub eligible_count: i32,
    pub reserved_quantity: i32,
    pub available_before_reservation: i64,
}

/// Command to close a release campaign.
#[derive(Clone, Debug)]
pub struct CloseReleaseCampaignCommand {
    pub workspace_id: Uuid,
    pub campaign_id: Uuid,
    pub idempotency_key: String,
    pub request_id: Option<String>,
}

/// Result of closing a release campaign.
#[derive(Clone, Debug)]
pub struct CloseReleaseCampaignResult {
    pub replayed: bool,
    pub already_closed: bool,
}

/// Command to update a release recipient's status.
#[derive(Clone, Debug)]
pub struct UpdateReleaseRecipientCommand {
    pub workspace_id: Uuid,
    pub campaign_id: Uuid,
    pub beacon_id: Uuid,
    pub status: String,
    pub idempotency_key: String,
    pub request_id: Option<String>,
}

/// Result of updating a release recipient.
#[derive(Clone, Debug)]
pub struct UpdateReleaseRecipientResult {
    pub replayed: bool,
}

/// Repository port for beacon release admin write operations.
#[async_trait]
pub trait BeaconReleaseAdminRepository: Send + Sync {
    /// Create a new release campaign in draft status.
    async fn create_release_campaign(
        &self,
        command: &CreateReleaseCampaignCommand,
    ) -> Result<CreateReleaseCampaignResult, BeaconReleaseAdminError>;

    /// Launch a campaign: reserve stock, insert recipients, queue outbox.
    async fn launch_release_campaign(
        &self,
        command: &LaunchReleaseCampaignCommand,
    ) -> Result<LaunchReleaseCampaignResult, BeaconReleaseAdminError>;

    /// Close a campaign and release stock reservation.
    async fn close_release_campaign(
        &self,
        command: &CloseReleaseCampaignCommand,
    ) -> Result<CloseReleaseCampaignResult, BeaconReleaseAdminError>;

    /// Update a recipient's status (prepared/sent/delivered/cancelled).
    async fn update_release_recipient(
        &self,
        command: &UpdateReleaseRecipientCommand,
    ) -> Result<UpdateReleaseRecipientResult, BeaconReleaseAdminError>;
}
