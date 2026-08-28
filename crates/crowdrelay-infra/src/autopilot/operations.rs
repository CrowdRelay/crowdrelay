//! Set-oriented PostgreSQL snapshot loaders for phase-2 ViryaOS bounded contexts.

use std::collections::HashMap;

use crowdrelay_domain::{
    BeaconId, ContentSourceId, EventId, ExperimentId, ExperimentVariantId, MerchProductId,
    OutreachOpportunityId, OutreachTargetId, WorkspaceId,
    beacons::{
        BeaconCampaignSnapshot, BeaconDiscoverySnapshot, BeaconInviteSnapshot, BeaconKind,
        BeaconReplyDisposition,
    },
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

use super::operator_actions::insert_operator_action;
use super::{
    MAX_SNAPSHOTS_PER_CONTEXT, PostgresAutopilotRepository, map_sqlx, parse_confidence,
    parse_context,
};
use crowdrelay_application::autopilot::{AutopilotControlMutation, RecordBookingReply};
use crowdrelay_application::{IdempotencyKey, RepositoryError, RequestId};

mod acquisition_channels;
mod chief;
mod discovery;
mod execution;
mod growth_debt;
mod growth_intelligence;
mod ingress;
mod next_best_action;
mod release_links;
mod reply_triage;
mod show_growth;
mod show_growth_execution;
mod snapshots;

pub(super) use acquisition_channels::*;
pub(super) use chief::*;
pub(super) use discovery::*;
pub(super) use execution::*;
pub(super) use growth_debt::*;
pub(super) use growth_intelligence::*;
pub(super) use next_best_action::*;
pub(super) use release_links::*;
pub(super) use show_growth::*;
pub(super) use show_growth_execution::*;
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
