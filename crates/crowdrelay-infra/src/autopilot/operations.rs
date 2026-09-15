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
    release_autopilot::ReleaseTier,
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
pub(in crate::autopilot) mod attribution;
pub(in crate::autopilot) mod belief_revisions;
mod chief;
mod discovery;
pub(in crate::autopilot) mod evidence;
mod execution;
pub(in crate::autopilot) mod experiment_assignments;
mod growth_debt;
pub(in crate::autopilot) mod growth_intelligence;
mod ingress;
mod next_best_action;
mod push_segments;
pub(in crate::autopilot) mod reach;
mod release_links;
mod release_r3_report;
pub(in crate::autopilot) mod reply_model;
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
pub(super) use push_segments::*;
pub(super) use release_links::*;
pub(super) use show_growth::*;
pub(super) use show_growth_execution::*;
pub(super) use snapshots::*;

/// The gate every smart-link destination must pass before it is inserted:
/// `smart_links.destination_url` is CHECKed `~* '^https?://'` — a looser gate
/// here turns a malformed URL into a CHECK violation that wedges the whole
/// milestone ladder on every retry, and a stricter one (`starts_with`, which
/// is case-sensitive) silently drops a valid `HTTPS://` URL.
fn is_http_url(url: &str) -> bool {
    url.get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("http://"))
        || url
            .get(..8)
            .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
}

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
