//! Booking-discovery ports: the venue/promoter supply pipeline, screened on
//! write. Split from `ports.rs` — the ratchet owns that file's ceiling and
//! this trait is self-contained.

use async_trait::async_trait;
use crowdrelay_domain::WorkspaceId;

use crate::RepositoryError;

/// Venue/promoter discovery: the booking pipeline's supply, screened on write.
#[async_trait]
pub trait AutopilotBookingDiscoveryRepository: Send + Sync {
    async fn ingest_booking_candidates(
        &self,
        workspace_id: WorkspaceId,
        candidates: Vec<crowdrelay_domain::booking_discovery::BookingCandidateInput>,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<BookingCandidateIngestion, RepositoryError>;

    /// Promotes one admitted email-route candidate into a real booking target.
    /// The human confirmation is what turns a published route into somebody
    /// the agent may approach.
    async fn confirm_booking_candidate(
        &self,
        workspace_id: WorkspaceId,
        candidate_id: crowdrelay_domain::OutreachOpportunityId,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<crate::autopilot::AutopilotControlMutation, RepositoryError>;

    async fn list_booking_candidates(
        &self,
        workspace_id: WorkspaceId,
        status: Option<String>,
        limit: u32,
    ) -> Result<Vec<BookingCandidateView>, RepositoryError>;
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct BookingCandidateIngestion {
    pub reported: u32,
    pub admitted: u32,
    pub refused: u32,
    /// Found through a second source: contact identity dedupes, so the same
    /// inbox is never two prospects.
    pub duplicates: u32,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct BookingCandidateView {
    pub candidate_id: uuid::Uuid,
    pub target_kind: String,
    pub display_name: String,
    pub city_slug: Option<String>,
    pub route_kind: String,
    pub route_value: String,
    pub source: String,
    pub fit_basis_points: u16,
    pub status: String,
    pub refusal_reason: Option<String>,
    pub booking_target_id: Option<uuid::Uuid>,
}
