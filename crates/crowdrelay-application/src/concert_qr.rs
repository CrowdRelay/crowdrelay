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

/// Marketing consent offered on the scan page.
///
/// The check-in itself never depends on it — attendance is a fact the room
/// counts either way. When the field is absent no consent row is written at
/// all, so "did not answer" never masquerades as "denied".
#[derive(Clone, Debug)]
pub struct CheckinConsent {
    pub granted: bool,
    pub policy_version: String,
}

/// Command to check a fan in to a concert via a campaign QR token.
#[derive(Clone, Debug)]
pub struct CheckinCommand {
    pub workspace_id: Uuid,
    pub event_slug: String,
    pub campaign_id: Uuid,
    pub event_id: Uuid,
    pub expires_at: i64,
    /// A live fan-session cookie is a verified identity. `None` means the
    /// scan came from a stranger's browser and `email` carries the claim.
    pub session_token: Option<String>,
    /// Normalized email a stranger typed on the scan page. Never mints a
    /// session: an unverified address must not authenticate as the fan behind
    /// it, so the follow-up is always an emailed token instead.
    pub email: Option<String>,
    pub consent: Option<CheckinConsent>,
    pub now: OffsetDateTime,
    pub request_id: Option<String>,
}

/// How the checked-in fan was identified.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckinIdentity {
    /// Verified by a live fan-session cookie.
    Session,
    /// Claimed by an email address; the inbox follow-up decides whether the
    /// claim becomes a reachable fan.
    EmailClaim,
}

impl CheckinIdentity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::EmailClaim => "email_claim",
        }
    }
}

/// Result of a fan check-in.
#[derive(Clone, Debug)]
pub struct CheckinResult {
    pub event_id: Uuid,
    pub event_slug: String,
    pub campaign_id: Uuid,
    pub created: bool,
    pub checked_in_at: OffsetDateTime,
    pub identity: CheckinIdentity,
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
