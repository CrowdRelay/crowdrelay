//! Concert QR repository port.
//!
//! The API layer retains token signing/verification, input validation and
//! response formatting. The adapter implementation owns the durable write
//! transactions: campaign creation, revocation and idempotent fan check-in.

use async_trait::async_trait;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

/// Error returned by concert QR repository operations.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ConcertQrError {
    #[error("concert QR resource not found")]
    NotFound,
    #[error("concert QR request conflicts with existing state")]
    Conflict,
    #[error("concert QR request is invalid")]
    Invalid,
    #[error("concert QR repository is temporarily unavailable")]
    Unavailable,
}

/// Public event information embedded in campaign results.
#[derive(Clone, Debug)]
pub struct ConcertEventInfo {
    pub id: Uuid,
    pub slug: String,
    pub title: String,
    pub venue: Option<String>,
    pub starts_at: OffsetDateTime,
}

/// Command to create a concert QR campaign for a published event.
#[derive(Clone, Debug)]
pub struct CreateCampaignCommand {
    pub workspace_id: Uuid,
    pub event_slug: String,
    pub label: String,
    pub valid_from: OffsetDateTime,
    pub valid_until: OffsetDateTime,
    pub max_checkins: Option<i32>,
    pub created_at: OffsetDateTime,
    pub request_id: Option<String>,
}

/// Result of creating a concert QR campaign.
#[derive(Clone, Debug)]
pub struct CreateCampaignResult {
    pub campaign_id: Uuid,
    pub event: ConcertEventInfo,
    pub created_at: OffsetDateTime,
}

/// Command to revoke a concert QR campaign.
#[derive(Clone, Debug)]
pub struct RevokeCampaignCommand {
    pub workspace_id: Uuid,
    pub campaign_id: Uuid,
    pub request_id: Option<String>,
}

/// Command to check a fan in to a concert via a campaign QR token.
#[derive(Clone, Debug)]
pub struct CheckinCommand {
    pub workspace_id: Uuid,
    pub event_slug: String,
    pub campaign_id: Uuid,
    pub event_id: Uuid,
    pub expires_at: i64,
    pub session_token: String,
    pub now: OffsetDateTime,
    pub request_id: Option<String>,
}

/// Result of a fan check-in.
#[derive(Clone, Debug)]
pub struct CheckinResult {
    pub event_id: Uuid,
    pub event_slug: String,
    pub campaign_id: Uuid,
    pub created: bool,
    pub checked_in_at: OffsetDateTime,
}

/// Repository port for concert QR write operations.
///
/// Each method encapsulates a full transaction (reads + writes) so the
/// API handler is pure token verification, input validation and response
/// formatting.
#[async_trait]
pub trait ConcertQrRepository: Send + Sync {
    /// Create a concert QR campaign for a published event.
    async fn create_campaign(
        &self,
        command: &CreateCampaignCommand,
    ) -> Result<CreateCampaignResult, ConcertQrError>;

    /// Revoke a concert QR campaign.
    async fn revoke_campaign(&self, command: &RevokeCampaignCommand) -> Result<(), ConcertQrError>;

    /// Idempotently check a fan in to a concert via a campaign QR token.
    async fn check_in(&self, command: &CheckinCommand) -> Result<CheckinResult, ConcertQrError>;
}
