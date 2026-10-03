//! Reply triage — first-party classification of inbound replies. Split out of
//! `ports.rs` under the source-size ratchet; re-exported as
//! `ports::reply_triage::*` like `booking_discovery`.

use async_trait::async_trait;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::outreach::OutreachTargetKind;
use time::OffsetDateTime;

use crate::RepositoryError;

// ---------------------------------------------------------------------------
// Reply triage — first-party classification of inbound replies.
//
// n8n posts replies with a disposition it assigned. When the disposition is
// `Received` (unclassified), the worker re-classifies using the domain
// classifier and records the result. Replies that need human review are
// surfaced via the operator brief.
// ---------------------------------------------------------------------------

/// What the reply's target is, at the granularity the triage loop needs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplyTargetKind {
    /// One of the outreach kinds — the classifier's vocabulary applies.
    Outreach(OutreachTargetKind),
    /// A promoter, venue, or festival on the booking channel. A negotiation
    /// reply is
    /// always a human's call: the operator filed the disposition with the
    /// reply, and the number inside the text is a proposal to confirm, not
    /// a disposition to infer.
    BookingCounterparty,
}

/// A reply awaiting first-party classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplyNeedingTriage {
    pub reply_id: uuid::Uuid,
    pub target_id: uuid::Uuid,
    pub target_kind: ReplyTargetKind,
    pub reply_text: String,
    pub previous_disposition: Option<crowdrelay_domain::outreach::OutreachReplyDisposition>,
}

/// The result of classifying one reply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplyTriageResult {
    pub classification: crowdrelay_domain::reply_triage::ReplyClassification,
    pub classified_at: OffsetDateTime,
}

#[async_trait]
pub trait AutopilotReplyTriageRepository: Send + Sync {
    /// Loads replies with `Received` disposition that have not been classified
    /// by the first-party classifier yet. Bounded by `limit`.
    async fn load_replies_needing_triage(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
    ) -> Result<Vec<ReplyNeedingTriage>, RepositoryError>;

    /// Records the classification for a reply and updates the reply's
    /// disposition if the classifier produced an auto-classification.
    /// For `NeedsHuman`, the disposition stays `Received` and the
    /// classification is stored for the operator brief to surface.
    async fn record_reply_classification(
        &self,
        workspace_id: WorkspaceId,
        reply_id: uuid::Uuid,
        result: &ReplyTriageResult,
    ) -> Result<(), RepositoryError>;
}
