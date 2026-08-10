//! Set-oriented PostgreSQL snapshot loaders for phase-2 ViryaOS bounded contexts.

use std::collections::HashMap;

use crowdrelay_domain::{
    ContentSourceId, EventId, ExperimentId, ExperimentVariantId, MerchProductId,
    OutreachOpportunityId, OutreachTargetId, WorkspaceId,
    booking::BookingReplyDisposition,
    campaign_lifecycle::{EventCampaignHistory, EventCampaignSnapshot},
    content_supply::{ContentArtifactKind, ContentSourceKind, ContentSupplySnapshot},
    experimentation::{ExperimentMetric, ExperimentSnapshot, ExperimentVariantSnapshot},
    merch_bundle::MerchBundleSnapshot,
    outreach::{OutreachReplyDisposition, OutreachSnapshot, OutreachTargetKind},
    show_operations::{ShowTaskKind, ShowTaskSnapshot},
};
use serde_json::{Value, json};
use sqlx::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;

use super::control::insert_operator_action;
use super::{
    MAX_SNAPSHOTS_PER_CONTEXT, PostgresAutopilotRepository, configure_transaction, map_sqlx,
    parse_confidence, parse_context,
};
use crowdrelay_application::autopilot::{AutopilotControlMutation, RecordBookingReply};
use crowdrelay_application::{IdempotencyKey, RepositoryError, RequestId};

mod chief;
mod execution;
mod ingress;
mod snapshots;

pub(super) use chief::*;
pub(super) use execution::*;
pub(super) use snapshots::*;

const fn booking_reply_str(value: BookingReplyDisposition) -> &'static str {
    match value {
        BookingReplyDisposition::None => "none",
        BookingReplyDisposition::Received => "received",
        BookingReplyDisposition::Positive => "positive",
        BookingReplyDisposition::Booked => "booked",
        BookingReplyDisposition::Declined => "declined",
        BookingReplyDisposition::DoNotContact => "do_not_contact",
    }
}
