//! Beacon Signal lifecycle repository port.
//!
//! The API layer's beacon signal handlers (invite creation, invite exchange,
//! preference updates, press requests, logout, nearby emission, engagement
//! recording, coverage submission, leave) previously contained SQL writes
//! directly. This port trait moves those writes behind a repository so the
//! API layer is pure protocol mapping + crypto.

use async_trait::async_trait;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

/// Error returned by beacon signal repository operations.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum BeaconSignalRepositoryError {
    #[error("beacon signal resource not found")]
    NotFound,
    #[error("beacon signal request conflicts with existing state")]
    Conflict,
    #[error("beacon signal request is invalid")]
    BadRequest,
    #[error("beacon signal repository is temporarily unavailable")]
    Unavailable,
}

/// Command to create or re-issue a beacon signal invite.
#[derive(Clone, Debug)]
pub struct CreateInviteCommand {
    pub workspace_id: Uuid,
    pub beacon_id: Uuid,
    pub invite_token_hash: Vec<u8>,
    pub invite_expires_at: OffsetDateTime,
    pub radius_km: i32,
    pub locale: String,
}

/// Result of creating a beacon signal invite.
#[derive(Clone, Debug)]
pub struct CreateInviteResult {
    pub display_name: String,
}

/// Command to exchange an invite token for an active session.
#[derive(Clone, Debug)]
pub struct ExchangeInviteCommand {
    pub workspace_id: Uuid,
    pub invite_token_hash: Vec<u8>,
    pub bearer_token_hash: Vec<u8>,
    pub session_id: Uuid,
    pub session_expires_at: OffsetDateTime,
    pub client_kind: String,
    pub locale: Option<String>,
    pub radius_km: Option<i32>,
    pub topics: Option<Vec<String>>,
}

/// Result of exchanging an invite token.
#[derive(Clone, Debug)]
pub struct ExchangeInviteResult {
    pub beacon_id: Uuid,
    pub display_name: String,
    pub beacon_kind: String,
    pub radius_km: i32,
    pub locale: String,
    pub topics: Vec<String>,
    pub nearby_gigs_enabled: bool,
}

/// Command to update beacon preferences.
#[derive(Clone, Debug)]
pub struct UpdatePreferencesCommand {
    pub workspace_id: Uuid,
    pub beacon_id: Uuid,
    pub radius_km: Option<i32>,
    pub locale: Option<String>,
    pub topics: Option<Vec<String>>,
    pub nearby_gigs_enabled: Option<bool>,
}

/// Updated beacon preferences.
#[derive(Clone, Debug)]
pub struct BeaconPreferences {
    pub radius_km: i32,
    pub locale: String,
    pub topics: Vec<String>,
    pub nearby_gigs_enabled: bool,
}

/// Command to create a press request.
#[derive(Clone, Debug)]
pub struct CreatePressRequestCommand {
    pub workspace_id: Uuid,
    pub beacon_id: Uuid,
    pub event_id: Option<Uuid>,
    pub request_kind: String,
    pub details: Option<String>,
    pub request_id_header: Option<String>,
}

/// Result of creating a press request.
#[derive(Clone, Debug)]
pub struct CreatePressRequestResult {
    pub request_id: Uuid,
}

/// Command to log out a beacon session.
#[derive(Clone, Debug)]
pub struct LogoutCommand {
    pub workspace_id: Uuid,
    pub session_hash: Vec<u8>,
}

/// Command to emit nearby concert notifications.
#[derive(Clone, Debug)]
pub struct EmitNearbyCommand {
    pub workspace_id: Uuid,
    pub limit: i64,
    pub lead_days: i64,
    pub push_enabled: bool,
}

/// Result of emitting nearby notifications.
#[derive(Clone, Debug)]
pub struct EmitNearbyResult {
    pub eligible: i64,
    pub push_queued: i64,
}

/// Command to record a beacon's event engagement.
#[derive(Clone, Debug)]
pub struct RecordEngagementCommand {
    pub workspace_id: Uuid,
    pub beacon_id: Uuid,
    pub event_id: Uuid,
    pub radius_km: i32,
    pub action: String,
    pub help_kind: Option<String>,
    pub help_details: Option<String>,
    pub request_id_header: Option<String>,
}

/// Result of recording an engagement.
#[derive(Clone, Debug)]
pub struct RecordEngagementResult {
    pub status: String,
    pub help_kind: Option<String>,
}

/// Command to submit coverage for an event.
#[derive(Clone, Debug)]
pub struct SubmitCoverageCommand {
    pub workspace_id: Uuid,
    pub beacon_id: Uuid,
    pub event_id: Uuid,
    pub coverage_kind: String,
    pub url: String,
    pub title: Option<String>,
    pub request_id_header: Option<String>,
}

/// Result of submitting coverage.
#[derive(Clone, Debug)]
pub struct SubmitCoverageResult {
    pub coverage_id: Uuid,
}

/// Command to leave the beacon signal program.
#[derive(Clone, Debug)]
pub struct LeaveCommand {
    pub workspace_id: Uuid,
    pub beacon_id: Uuid,
    pub do_not_contact: bool,
    pub request_id_header: Option<String>,
}

/// Repository port for beacon signal lifecycle write operations.
///
/// Each method encapsulates a full transaction (reads + writes) so the
/// API handler is pure input validation, crypto, and response formatting.
#[async_trait]
pub trait BeaconSignalRepository: Send + Sync {
    /// Create or re-issue an invite for a beacon. Revokes old sessions.
    async fn create_invite(
        &self,
        command: &CreateInviteCommand,
    ) -> Result<CreateInviteResult, BeaconSignalRepositoryError>;

    /// Exchange an invite token for an active session.
    async fn exchange_invite(
        &self,
        command: &ExchangeInviteCommand,
    ) -> Result<ExchangeInviteResult, BeaconSignalRepositoryError>;

    /// Update beacon preferences (radius, locale, topics, nearby_gigs_enabled).
    async fn update_preferences(
        &self,
        command: &UpdatePreferencesCommand,
    ) -> Result<Option<BeaconPreferences>, BeaconSignalRepositoryError>;

    /// Create a press request and queue the outbox event.
    async fn create_press_request(
        &self,
        command: &CreatePressRequestCommand,
    ) -> Result<CreatePressRequestResult, BeaconSignalRepositoryError>;

    /// Revoke a beacon session and invalidate push endpoints.
    async fn logout(&self, command: &LogoutCommand) -> Result<(), BeaconSignalRepositoryError>;

    /// Emit nearby concert notifications (campaigns, engagements, push).
    async fn emit_nearby(
        &self,
        command: &EmitNearbyCommand,
    ) -> Result<EmitNearbyResult, BeaconSignalRepositoryError>;

    /// Record a beacon's event engagement and sync campaign state.
    async fn record_event_engagement(
        &self,
        command: &RecordEngagementCommand,
    ) -> Result<RecordEngagementResult, BeaconSignalRepositoryError>;

    /// Submit coverage for an event and mark engagement as completed.
    async fn submit_coverage(
        &self,
        command: &SubmitCoverageCommand,
    ) -> Result<SubmitCoverageResult, BeaconSignalRepositoryError>;

    /// Leave the beacon signal program (revoke profile, sessions, push).
    async fn leave(&self, command: &LeaveCommand) -> Result<(), BeaconSignalRepositoryError>;
}
