//! Local Signal beacons: media and industry relationships that amplify one show locally.
//!
//! A `Beacon` is not a fan and not a generic CRM contact. It is a verified local
//! amplifier around a concrete market or event: radio, local press, TV, reviewer,
//! photographer, promoter, venue, scene partner, patron or community partner. CrowdRelay owns selection,
//! cadence, consent/suppression and measurable relationship state; n8n/Gemini may
//! execute/personalise an already-authorised message but never choose the recipient.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{BeaconId, EventId, autonomy::Confidence};

/// Canonical identity used when discovery reconciles a contact with an existing Beacon.
///
/// A normalized e-mail address is authoritative when present. Destination URL is
/// only the fallback for contacts without e-mail. Keeping this rule in the domain
/// prevents API adapters and database locks from drifting apart.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BeaconContactIdentity<'a> {
    Email(&'a str),
    DestinationUrl(&'a str),
}

impl<'a> BeaconContactIdentity<'a> {
    #[must_use]
    pub fn from_normalized(
        contact_email: Option<&'a str>,
        destination_url: Option<&'a str>,
    ) -> Option<Self> {
        contact_email
            .filter(|value| !value.is_empty())
            .map(Self::Email)
            .or_else(|| {
                destination_url
                    .filter(|value| !value.is_empty())
                    .map(Self::DestinationUrl)
            })
    }

    #[must_use]
    pub const fn namespace(self) -> &'static str {
        match self {
            Self::Email(_) => "email",
            Self::DestinationUrl(_) => "destination",
        }
    }

    #[must_use]
    pub const fn value(self) -> &'a str {
        match self {
            Self::Email(value) | Self::DestinationUrl(value) => value,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BeaconKind {
    Radio,
    LocalPress,
    Television,
    Reviewer,
    Creator,
    Photographer,
    Promoter,
    Venue,
    ScenePartner,
    Patron,
    Community,
}

impl BeaconKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Radio => "radio",
            Self::LocalPress => "local_press",
            Self::Television => "television",
            Self::Reviewer => "reviewer",
            Self::Creator => "creator",
            Self::Photographer => "photographer",
            Self::Promoter => "promoter",
            Self::Venue => "venue",
            Self::ScenePartner => "scene_partner",
            Self::Patron => "patron",
            Self::Community => "community",
        }
    }

    /// Collaboration offers that are naturally useful for this kind of local amplifier.
    /// Executors may personalise the wording but must not invent a different business intent.
    #[must_use]
    pub const fn preferred_offer_keys(self) -> &'static [&'static str] {
        match self {
            Self::Radio => &["airplay", "interview", "ticket_giveaway", "patronage"],
            Self::LocalPress => &[
                "preview",
                "interview",
                "review",
                "patronage",
                "ticket_giveaway",
            ],
            Self::Television => &["interview", "local_segment"],
            Self::Reviewer => &["review", "interview", "live_review"],
            Self::Creator => &["co_post", "short_form_clip", "ticket_giveaway"],
            Self::Photographer => &["photo_access", "live_gallery"],
            Self::Promoter => &["support_slot", "future_booking", "cross_promo"],
            Self::Venue => &[
                "event_listing",
                "co_post",
                "newsletter",
                "cross_promo",
                "ticket_giveaway",
            ],
            Self::ScenePartner => &[
                "cross_promo",
                "community_listing",
                "ticket_giveaway",
                "support_exchange",
            ],
            Self::Patron => &["patronage", "preview", "ticket_giveaway"],
            Self::Community => &["community_listing", "cross_promo", "ticket_giveaway"],
        }
    }

    /// Narrow the offer menu to the current campaign phase. A professional
    /// promoter does not send the same ask three times: early contact should
    /// create editorial/context opportunities, the middle wave should secure a
    /// concrete collaboration, the last wave should make attendance easy, and
    /// post-show contact should compound proof/relationship value.
    #[must_use]
    pub const fn offer_keys_for_phase(self, phase: BeaconOutreachPhase) -> &'static [&'static str] {
        match phase {
            BeaconOutreachPhase::Initial => match self {
                Self::Radio => &["airplay", "interview", "patronage"],
                Self::LocalPress => &["preview", "interview", "patronage"],
                Self::Television => &["interview", "local_segment"],
                Self::Reviewer => &["review", "interview"],
                Self::Creator => &["co_post", "short_form_clip"],
                Self::Photographer => &["photo_access"],
                Self::Promoter => &["support_slot", "future_booking", "cross_promo"],
                Self::Venue => &["event_listing", "newsletter", "co_post"],
                Self::ScenePartner => &["community_listing", "cross_promo", "support_exchange"],
                Self::Patron => &["patronage", "preview"],
                Self::Community => &["community_listing", "cross_promo"],
            },
            BeaconOutreachPhase::CollaborationFollowUp => match self {
                Self::Radio => &["interview", "ticket_giveaway", "patronage"],
                Self::LocalPress => &["interview", "ticket_giveaway", "patronage"],
                Self::Television => &["interview", "local_segment"],
                Self::Reviewer => &["interview", "review"],
                Self::Creator => &["co_post", "short_form_clip", "ticket_giveaway"],
                Self::Photographer => &["photo_access", "live_gallery"],
                Self::Promoter => &["cross_promo", "support_slot", "future_booking"],
                Self::Venue => &["newsletter", "co_post", "ticket_giveaway", "cross_promo"],
                Self::ScenePartner => &["cross_promo", "ticket_giveaway", "support_exchange"],
                Self::Patron => &["patronage", "ticket_giveaway", "preview"],
                Self::Community => &["cross_promo", "ticket_giveaway", "community_listing"],
            },
            BeaconOutreachPhase::LocalPush => match self {
                Self::Radio => &["ticket_giveaway", "airplay", "interview"],
                Self::LocalPress => &["ticket_giveaway", "preview"],
                Self::Television => &["local_segment", "interview"],
                Self::Reviewer => &["live_review"],
                Self::Creator => &["short_form_clip", "ticket_giveaway", "co_post"],
                Self::Photographer => &["photo_access"],
                Self::Promoter => &["cross_promo"],
                Self::Venue => &["co_post", "newsletter", "ticket_giveaway"],
                Self::ScenePartner => &["cross_promo", "ticket_giveaway"],
                Self::Patron => &["ticket_giveaway", "preview"],
                Self::Community => &["ticket_giveaway", "community_listing", "cross_promo"],
            },
            BeaconOutreachPhase::PostShowThanks => match self {
                Self::Radio => &["relationship_thanks"],
                Self::LocalPress => &["review", "relationship_thanks"],
                Self::Television => &["relationship_thanks"],
                Self::Reviewer => &["live_review", "relationship_thanks"],
                Self::Creator => &["post_show_recap", "relationship_thanks"],
                Self::Photographer => &["live_gallery", "relationship_thanks"],
                Self::Promoter => &["future_booking", "relationship_thanks"],
                Self::Venue => &["post_show_recap", "relationship_thanks"],
                Self::ScenePartner => &["post_show_recap", "relationship_thanks"],
                Self::Patron => &["relationship_thanks", "review"],
                Self::Community => &["post_show_recap", "relationship_thanks"],
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BeaconReplyDisposition {
    None,
    Received,
    Interested,
    Partner,
    Declined,
    DoNotContact,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BeaconOutreachPhase {
    Initial,
    CollaborationFollowUp,
    LocalPush,
    PostShowThanks,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct BeaconDiscoverySnapshot {
    pub event_id: EventId,
    pub event_starts_at: OffsetDateTime,
    pub known_local_beacons: u16,
    pub last_discovery_at: Option<OffsetDateTime>,
    pub in_flight: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BeaconDiscoveryDecision {
    Hold(BeaconDiscoveryHoldReason),
    Request {
        target_count: u16,
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BeaconDiscoveryHoldReason {
    InvalidPolicy,
    InvalidSnapshot,
    InFlight,
    TooEarly,
    EnoughKnown,
    RecentlyScouted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct BeaconCampaignSnapshot {
    pub beacon_id: BeaconId,
    pub beacon_version: i64,
    pub event_id: EventId,
    pub kind: BeaconKind,
    pub active: bool,
    pub verified: bool,
    pub accepts_outreach: bool,
    pub do_not_contact: bool,
    pub relationship_score: u16,
    pub relevance_basis_points: u16,
    pub evidence_confidence: Confidence,
    pub event_starts_at: OffsetDateTime,
    pub last_outreach_at: Option<OffsetDateTime>,
    pub followup_count: u16,
    pub last_reply: BeaconReplyDisposition,
    pub in_flight: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct BeaconCampaignPolicy {
    /// Earliest lead time at which local beacon activity is useful.
    pub discovery_lead_days: u32,
    /// Minimum verified local Beacons we want available before outreach starts.
    pub minimum_local_beacons: u16,
    /// Do not repeatedly re-scout the same market while external discovery is fresh.
    pub discovery_refresh_days: u32,
    /// First real pitch. Default ≈ six weeks before the show.
    pub initial_lead_days: u32,
    /// Collaboration/patronage follow-up. Default ≈ four weeks before.
    pub collaboration_lead_days: u32,
    /// Final local relevance wave. Default ≈ two weeks before.
    pub local_push_lead_days: u32,
    /// Thank-you/relationship close-loop window after the show.
    pub post_show_thanks_days: u32,
    pub minimum_relevance_basis_points: u16,
    pub minimum_confidence_basis_points: u16,
    pub maximum_pre_show_touches: u16,
    pub minimum_contact_gap_days: u32,
}

impl Default for BeaconCampaignPolicy {
    fn default() -> Self {
        Self {
            discovery_lead_days: 60,
            minimum_local_beacons: 8,
            discovery_refresh_days: 14,
            initial_lead_days: 42,
            collaboration_lead_days: 28,
            local_push_lead_days: 14,
            post_show_thanks_days: 5,
            minimum_relevance_basis_points: 7_000,
            minimum_confidence_basis_points: 7_000,
            maximum_pre_show_touches: 3,
            minimum_contact_gap_days: 7,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BeaconDecision {
    Hold(BeaconHoldReason),
    Request {
        phase: BeaconOutreachPhase,
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BeaconHoldReason {
    InvalidPolicy,
    InvalidSnapshot,
    Ineligible,
    LowRelevance,
    AlreadyReplied,
    InFlight,
    TooEarly,
    NotDue,
    TouchLimit,
}

#[must_use]
pub fn evaluate_beacon_discovery(
    snapshot: BeaconDiscoverySnapshot,
    policy: BeaconCampaignPolicy,
    now: OffsetDateTime,
) -> BeaconDiscoveryDecision {
    if !valid_policy(policy) {
        return BeaconDiscoveryDecision::Hold(BeaconDiscoveryHoldReason::InvalidPolicy);
    }
    if snapshot.event_starts_at <= now {
        return BeaconDiscoveryDecision::Hold(BeaconDiscoveryHoldReason::InvalidSnapshot);
    }
    if snapshot.in_flight {
        return BeaconDiscoveryDecision::Hold(BeaconDiscoveryHoldReason::InFlight);
    }
    let days_until_show = (snapshot.event_starts_at - now).whole_days();
    if days_until_show > i64::from(policy.discovery_lead_days) {
        return BeaconDiscoveryDecision::Hold(BeaconDiscoveryHoldReason::TooEarly);
    }
    if snapshot.known_local_beacons >= policy.minimum_local_beacons {
        return BeaconDiscoveryDecision::Hold(BeaconDiscoveryHoldReason::EnoughKnown);
    }
    if snapshot.last_discovery_at.is_some_and(|at| {
        at > now || now - at < Duration::days(i64::from(policy.discovery_refresh_days))
    }) {
        return BeaconDiscoveryDecision::Hold(BeaconDiscoveryHoldReason::RecentlyScouted);
    }

    BeaconDiscoveryDecision::Request {
        target_count: policy
            .minimum_local_beacons
            .saturating_sub(snapshot.known_local_beacons)
            .max(1),
        confidence: Confidence::saturating_from_basis_points(9_000),
    }
}

#[must_use]
pub fn evaluate_beacon_campaign(
    snapshot: BeaconCampaignSnapshot,
    policy: BeaconCampaignPolicy,
    now: OffsetDateTime,
) -> BeaconDecision {
    if !valid_policy(policy) {
        return BeaconDecision::Hold(BeaconHoldReason::InvalidPolicy);
    }
    if snapshot.event_starts_at < now - Duration::days(i64::from(policy.post_show_thanks_days))
        || snapshot.relationship_score > 100
    {
        return BeaconDecision::Hold(BeaconHoldReason::InvalidSnapshot);
    }
    if !snapshot.active
        || !snapshot.verified
        || !snapshot.accepts_outreach
        || snapshot.do_not_contact
    {
        return BeaconDecision::Hold(BeaconHoldReason::Ineligible);
    }
    if snapshot.relevance_basis_points < policy.minimum_relevance_basis_points
        || snapshot.evidence_confidence.basis_points() < policy.minimum_confidence_basis_points
    {
        return BeaconDecision::Hold(BeaconHoldReason::LowRelevance);
    }
    if snapshot.in_flight {
        return BeaconDecision::Hold(BeaconHoldReason::InFlight);
    }
    if matches!(snapshot.last_reply, BeaconReplyDisposition::DoNotContact) {
        return BeaconDecision::Hold(BeaconHoldReason::Ineligible);
    }
    if matches!(
        snapshot.last_reply,
        BeaconReplyDisposition::Interested | BeaconReplyDisposition::Partner
    ) && now < snapshot.event_starts_at
    {
        // Once a real relationship is active for this event, operational tasks
        // should take over; do not keep pitching the same partner automatically.
        return BeaconDecision::Hold(BeaconHoldReason::AlreadyReplied);
    }

    let until_show = snapshot.event_starts_at - now;
    let days_until_show = until_show.whole_days();
    if days_until_show > i64::from(policy.discovery_lead_days) {
        return BeaconDecision::Hold(BeaconHoldReason::TooEarly);
    }

    if days_until_show < 0 {
        if -days_until_show > i64::from(policy.post_show_thanks_days) {
            return BeaconDecision::Hold(BeaconHoldReason::NotDue);
        }
        if snapshot
            .last_outreach_at
            .is_some_and(|at| at >= snapshot.event_starts_at)
        {
            return BeaconDecision::Hold(BeaconHoldReason::AlreadyReplied);
        }
        return request(BeaconOutreachPhase::PostShowThanks, snapshot);
    }

    if snapshot.followup_count >= policy.maximum_pre_show_touches {
        return BeaconDecision::Hold(BeaconHoldReason::TouchLimit);
    }
    if snapshot.last_outreach_at.is_some_and(|at| {
        at > now || now - at < Duration::days(i64::from(policy.minimum_contact_gap_days))
    }) {
        return BeaconDecision::Hold(BeaconHoldReason::NotDue);
    }

    let phase = if snapshot.last_outreach_at.is_none()
        && days_until_show <= i64::from(policy.initial_lead_days)
    {
        Some(BeaconOutreachPhase::Initial)
    } else if days_until_show <= i64::from(policy.local_push_lead_days) {
        Some(BeaconOutreachPhase::LocalPush)
    } else if days_until_show <= i64::from(policy.collaboration_lead_days) {
        Some(BeaconOutreachPhase::CollaborationFollowUp)
    } else {
        None
    };

    phase.map_or(BeaconDecision::Hold(BeaconHoldReason::NotDue), |phase| {
        request(phase, snapshot)
    })
}

fn request(phase: BeaconOutreachPhase, snapshot: BeaconCampaignSnapshot) -> BeaconDecision {
    let relationship_bonus = snapshot.relationship_score.saturating_mul(10).min(800);
    let relevance_bonus = snapshot
        .relevance_basis_points
        .saturating_sub(7_000)
        .min(800);
    let confidence = snapshot
        .evidence_confidence
        .basis_points()
        .saturating_add(relationship_bonus)
        .saturating_add(relevance_bonus)
        .min(10_000);
    BeaconDecision::Request {
        phase,
        confidence: Confidence::saturating_from_basis_points(confidence),
    }
}

#[must_use]
const fn valid_policy(policy: BeaconCampaignPolicy) -> bool {
    policy.discovery_lead_days >= policy.initial_lead_days
        && policy.minimum_local_beacons > 0
        && policy.minimum_local_beacons <= 50
        && policy.discovery_refresh_days > 0
        && policy.discovery_refresh_days <= policy.discovery_lead_days
        && policy.initial_lead_days >= policy.collaboration_lead_days
        && policy.collaboration_lead_days >= policy.local_push_lead_days
        && policy.local_push_lead_days > 0
        && policy.post_show_thanks_days > 0
        && policy.minimum_relevance_basis_points <= 10_000
        && policy.minimum_confidence_basis_points <= 10_000
        && policy.maximum_pre_show_touches > 0
        && policy.maximum_pre_show_touches <= 4
        && policy.minimum_contact_gap_days > 0
}

#[cfg(test)]
mod tests {
    use super::BeaconContactIdentity;

    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    fn snapshot(days_until_show: i64) -> BeaconCampaignSnapshot {
        BeaconCampaignSnapshot {
            beacon_id: BeaconId::new(),
            beacon_version: 1,
            event_id: EventId::new(),
            kind: BeaconKind::Radio,
            active: true,
            verified: true,
            accepts_outreach: true,
            do_not_contact: false,
            relationship_score: 60,
            relevance_basis_points: 8_800,
            evidence_confidence: Confidence::saturating_from_basis_points(8_500),
            event_starts_at: now() + Duration::days(days_until_show),
            last_outreach_at: None,
            followup_count: 0,
            last_reply: BeaconReplyDisposition::None,
            in_flight: false,
        }
    }

    #[test]
    fn discovery_requests_only_the_missing_verified_local_supply() {
        let policy = BeaconCampaignPolicy::default();
        let discovery = BeaconDiscoverySnapshot {
            event_id: EventId::new(),
            event_starts_at: now() + Duration::days(55),
            known_local_beacons: 3,
            last_discovery_at: None,
            in_flight: false,
        };
        assert!(matches!(
            evaluate_beacon_discovery(discovery, policy, now()),
            BeaconDiscoveryDecision::Request {
                target_count: 5,
                ..
            }
        ));
    }

    #[test]
    fn discovery_is_quiet_when_supply_is_fresh_or_sufficient() {
        let policy = BeaconCampaignPolicy::default();
        let mut discovery = BeaconDiscoverySnapshot {
            event_id: EventId::new(),
            event_starts_at: now() + Duration::days(50),
            known_local_beacons: policy.minimum_local_beacons,
            last_discovery_at: None,
            in_flight: false,
        };
        assert_eq!(
            evaluate_beacon_discovery(discovery, policy, now()),
            BeaconDiscoveryDecision::Hold(BeaconDiscoveryHoldReason::EnoughKnown)
        );
        discovery.known_local_beacons = 0;
        discovery.last_discovery_at = Some(now() - Duration::days(2));
        assert_eq!(
            evaluate_beacon_discovery(discovery, policy, now()),
            BeaconDiscoveryDecision::Hold(BeaconDiscoveryHoldReason::RecentlyScouted)
        );
    }

    #[test]
    fn initial_pitch_is_due_around_six_weeks_before_show() {
        assert!(matches!(
            evaluate_beacon_campaign(snapshot(40), BeaconCampaignPolicy::default(), now()),
            BeaconDecision::Request {
                phase: BeaconOutreachPhase::Initial,
                ..
            }
        ));
    }

    #[test]
    fn beacon_never_contacts_unverified_or_suppressed_target() {
        let mut candidate = snapshot(30);
        candidate.verified = false;
        assert_eq!(
            evaluate_beacon_campaign(candidate, BeaconCampaignPolicy::default(), now()),
            BeaconDecision::Hold(BeaconHoldReason::Ineligible)
        );
        candidate.verified = true;
        candidate.do_not_contact = true;
        assert_eq!(
            evaluate_beacon_campaign(candidate, BeaconCampaignPolicy::default(), now()),
            BeaconDecision::Hold(BeaconHoldReason::Ineligible)
        );
    }

    #[test]
    fn established_partner_is_not_repitched_before_same_show() {
        let mut candidate = snapshot(12);
        candidate.last_reply = BeaconReplyDisposition::Partner;
        assert_eq!(
            evaluate_beacon_campaign(candidate, BeaconCampaignPolicy::default(), now()),
            BeaconDecision::Hold(BeaconHoldReason::AlreadyReplied)
        );
    }

    #[test]
    fn beacon_offers_change_with_campaign_phase() {
        let early = BeaconKind::LocalPress.offer_keys_for_phase(BeaconOutreachPhase::Initial);
        let last = BeaconKind::LocalPress.offer_keys_for_phase(BeaconOutreachPhase::LocalPush);
        let post = BeaconKind::Reviewer.offer_keys_for_phase(BeaconOutreachPhase::PostShowThanks);
        assert!(early.contains(&"interview") && early.contains(&"patronage"));
        assert!(last.contains(&"ticket_giveaway"));
        assert!(post.contains(&"live_review"));
        assert!(!post.contains(&"ticket_giveaway"));
    }
    #[test]
    fn post_show_thanks_closes_relationship_loop_once() {
        let candidate = snapshot(-2);
        assert!(matches!(
            evaluate_beacon_campaign(candidate, BeaconCampaignPolicy::default(), now()),
            BeaconDecision::Request {
                phase: BeaconOutreachPhase::PostShowThanks,
                ..
            }
        ));
    }

    #[test]
    fn contact_identity_prefers_email_and_falls_back_to_destination() {
        assert_eq!(
            BeaconContactIdentity::from_normalized(
                Some("media@example.com"),
                Some("https://media.example/")
            ),
            Some(BeaconContactIdentity::Email("media@example.com")),
        );
        assert_eq!(
            BeaconContactIdentity::from_normalized(None, Some("https://media.example/")),
            Some(BeaconContactIdentity::DestinationUrl(
                "https://media.example/"
            )),
        );
        assert_eq!(BeaconContactIdentity::from_normalized(None, None), None);
    }
}
