//! Local Signal beacons: media and industry relationships that amplify one show locally.
//!
//! A `Beacon` is not a fan and not a generic CRM contact. It is a verified local
//! amplifier around a concrete market or event: radio, local press, TV, reviewer,
//! photographer, promoter, venue, scene partner, patron or community partner. CrowdRelay owns selection,
//! cadence, consent/suppression and measurable relationship state; n8n/Gemini may
//! execute/personalise an already-authorised message but never choose the recipient.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{BeaconId, CityId, EventId, autonomy::Confidence};

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
    /// Activated fans a city needs before it is worth scouting for scene nodes
    /// without a show booked there. Measured in people who did something, not
    /// signups: a hundred dormant accounts is not a warm city.
    pub minimum_activated_fans_for_city_scout: u32,
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
    /// Scene-node invite batches, one per beacon per show.
    pub invite: BeaconInvitePolicy,
}

impl Default for BeaconCampaignPolicy {
    fn default() -> Self {
        Self {
            discovery_lead_days: 60,
            minimum_local_beacons: 8,
            discovery_refresh_days: 14,
            // Ten active people in a city is a scene worth asking about and a
            // number a band can reach organically. Higher would mean never
            // scouting until the city no longer needs it.
            minimum_activated_fans_for_city_scout: 10,
            initial_lead_days: 42,
            collaboration_lead_days: 28,
            local_push_lead_days: 14,
            post_show_thanks_days: 5,
            minimum_relevance_basis_points: 7_000,
            minimum_confidence_basis_points: 7_000,
            maximum_pre_show_touches: 3,
            minimum_contact_gap_days: 7,
            invite: BeaconInvitePolicy::default(),
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

/// A city worth scouting for scene nodes, as the adapter can see it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CityBeaconSnapshot {
    pub city_id: CityId,
    /// Fans in this city who did something meaningful in the last 30 days.
    /// Real people, not signups — a city full of dormant accounts is not warm.
    pub activated_fans: u32,
    pub known_local_beacons: u16,
    pub last_discovery_at: Option<OffsetDateTime>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CityBeaconDecision {
    Hold(BeaconDiscoveryHoldReason),
    Request {
        target_count: u16,
        confidence: Confidence,
    },
}

/// Decides whether a city is warm enough to go looking for scene nodes in it.
///
/// The show-scoped rule above only fires inside the lead time of a booked gig,
/// which means a band with no shows can never find the people who would help it
/// get one. This is the other direction and the one a campaign needs: a city
/// with real active fans and no local scene nodes is worth scouting *before*
/// anything is booked there.
///
/// Warmth is measured in activated fans rather than signups on purpose. A
/// hundred dormant accounts in Kraków is not a reason to go looking for
/// promoters there, and treating it as one would send the agent hunting in
/// cities that only look busy.
#[must_use]
pub fn evaluate_city_beacon_discovery(
    snapshot: CityBeaconSnapshot,
    policy: BeaconCampaignPolicy,
    now: OffsetDateTime,
) -> CityBeaconDecision {
    if !valid_policy(policy) || policy.minimum_activated_fans_for_city_scout == 0 {
        return CityBeaconDecision::Hold(BeaconDiscoveryHoldReason::InvalidPolicy);
    }
    if snapshot.activated_fans < policy.minimum_activated_fans_for_city_scout {
        return CityBeaconDecision::Hold(BeaconDiscoveryHoldReason::TooEarly);
    }
    if snapshot.known_local_beacons >= policy.minimum_local_beacons {
        return CityBeaconDecision::Hold(BeaconDiscoveryHoldReason::EnoughKnown);
    }
    if snapshot.last_discovery_at.is_some_and(|at| {
        at > now || now - at < Duration::days(i64::from(policy.discovery_refresh_days))
    }) {
        return CityBeaconDecision::Hold(BeaconDiscoveryHoldReason::RecentlyScouted);
    }
    CityBeaconDecision::Request {
        target_count: policy
            .minimum_local_beacons
            .saturating_sub(snapshot.known_local_beacons)
            .max(1),
        confidence: Confidence::saturating_from_basis_points(8_800),
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

    #[test]
    fn a_warm_city_with_no_scene_nodes_is_scouted_without_a_show() {
        // The show-scoped rule can never fire for a band with no shows, which
        // is exactly the band that needs scene nodes most.
        let decision = evaluate_city_beacon_discovery(
            CityBeaconSnapshot {
                city_id: CityId::new(),
                activated_fans: 25,
                known_local_beacons: 1,
                last_discovery_at: None,
            },
            BeaconCampaignPolicy::default(),
            now(),
        );
        assert!(matches!(decision, CityBeaconDecision::Request { .. }));
    }

    #[test]
    fn a_city_of_dormant_accounts_is_not_warm() {
        // Signups would say this city is busy. Activated fans say nobody there
        // has done anything, and sending the agent hunting there wastes the
        // scouting budget on a number that only looks good.
        assert_eq!(
            evaluate_city_beacon_discovery(
                CityBeaconSnapshot {
                    city_id: CityId::new(),
                    activated_fans: 0,
                    known_local_beacons: 0,
                    last_discovery_at: None,
                },
                BeaconCampaignPolicy::default(),
                now(),
            ),
            CityBeaconDecision::Hold(BeaconDiscoveryHoldReason::TooEarly)
        );
    }

    #[test]
    fn a_city_that_already_has_its_scene_nodes_is_left_alone() {
        let policy = BeaconCampaignPolicy::default();
        assert_eq!(
            evaluate_city_beacon_discovery(
                CityBeaconSnapshot {
                    city_id: CityId::new(),
                    activated_fans: 100,
                    known_local_beacons: policy.minimum_local_beacons,
                    last_discovery_at: None,
                },
                policy,
                now(),
            ),
            CityBeaconDecision::Hold(BeaconDiscoveryHoldReason::EnoughKnown)
        );
    }

    #[test]
    fn a_recently_scouted_city_is_not_scouted_again() {
        let policy = BeaconCampaignPolicy::default();
        assert_eq!(
            evaluate_city_beacon_discovery(
                CityBeaconSnapshot {
                    city_id: CityId::new(),
                    activated_fans: 100,
                    known_local_beacons: 0,
                    last_discovery_at: Some(now() - Duration::days(1)),
                },
                policy,
                now(),
            ),
            CityBeaconDecision::Hold(BeaconDiscoveryHoldReason::RecentlyScouted)
        );

        assert!(matches!(
            evaluate_city_beacon_discovery(
                CityBeaconSnapshot {
                    city_id: CityId::new(),
                    activated_fans: 100,
                    known_local_beacons: 0,
                    last_discovery_at: Some(
                        now() - Duration::days(i64::from(policy.discovery_refresh_days))
                    ),
                },
                policy,
                now(),
            ),
            CityBeaconDecision::Request { .. }
        ));
    }

    #[test]
    fn the_scout_asks_only_for_the_shortfall() {
        let policy = BeaconCampaignPolicy::default();
        let CityBeaconDecision::Request { target_count, .. } = evaluate_city_beacon_discovery(
            CityBeaconSnapshot {
                city_id: CityId::new(),
                activated_fans: 100,
                known_local_beacons: policy.minimum_local_beacons - 3,
                last_discovery_at: None,
            },
            policy,
            now(),
        ) else {
            panic!("expected a scouting request");
        };
        assert_eq!(target_count, 3);
    }
}

/// One verified scene node, one of their city's upcoming shows, and how warm
/// the relationship is. Everything the invite rule needs and nothing it does
/// not: the adapter reports facts, the policy owns every threshold.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct BeaconInviteSnapshot {
    pub beacon_id: BeaconId,
    pub beacon_version: i64,
    pub event_id: EventId,
    pub kind: BeaconKind,
    pub active: bool,
    pub verified: bool,
    pub accepts_outreach: bool,
    pub do_not_contact: bool,
    pub relationship_score: u16,
    /// Hours until their city's show. Negative once it has played.
    pub hours_until_event: i64,
    /// Hours since this beacon was last asked to run an invite batch.
    /// `None` when they never have been.
    pub hours_since_last_invite_batch: Option<u32>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct BeaconInvitePolicy {
    /// Ask inside this window before the show. Earlier is noise — plans move;
    /// later is useless — codes take days to turn into people.
    pub invite_lead_days: u32,
    /// One ask per show per beacon past this gap. Scene nodes are finite
    /// relationships; the band gets one first approach to each of them.
    pub invite_cooldown_days: u32,
    /// Below this warmth a scene node is a name on a list, not a partner.
    pub minimum_relationship_score: u16,
    pub max_invites_per_batch: u16,
}

impl Default for BeaconInvitePolicy {
    fn default() -> Self {
        Self {
            invite_lead_days: 21,
            invite_cooldown_days: 30,
            minimum_relationship_score: 60,
            max_invites_per_batch: 25,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BeaconInviteDecision {
    Hold(BeaconInviteHoldReason),
    Request {
        requested_count: u16,
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BeaconInviteHoldReason {
    InvalidPolicy,
    Ineligible,
    LowWarmth,
    NotDue,
    WindowClosed,
    OnCooldown,
}

/// Decides whether one scene node should be asked to run invites for one show.
///
/// This is the acquisition half the system was missing: beacons exist to reach
/// rooms the band cannot, the invite machinery already works, and nothing ever
/// connected them. The ask rides an existing relationship (warmth gate), is
/// scoped to their own city's show (relevance), and is bounded (one batch,
/// cooled down) so a helpful partner is never worked like a mailing list.
///
/// Invite codes are first-party by construction — every signup they produce
/// arrives attributed and consented — but the *ask* is still third-party
/// contact, and its class says so.
#[must_use]
pub fn evaluate_beacon_invite_batch(
    snapshot: BeaconInviteSnapshot,
    policy: BeaconInvitePolicy,
) -> BeaconInviteDecision {
    if policy.invite_lead_days == 0 || policy.max_invites_per_batch == 0 {
        return BeaconInviteDecision::Hold(BeaconInviteHoldReason::InvalidPolicy);
    }
    if !snapshot.active
        || !snapshot.verified
        || !snapshot.accepts_outreach
        || snapshot.do_not_contact
    {
        return BeaconInviteDecision::Hold(BeaconInviteHoldReason::Ineligible);
    }
    if snapshot.relationship_score < policy.minimum_relationship_score {
        return BeaconInviteDecision::Hold(BeaconInviteHoldReason::LowWarmth);
    }
    if snapshot
        .hours_since_last_invite_batch
        .is_some_and(|hours| hours < u32::saturating_mul(policy.invite_cooldown_days, 24))
    {
        return BeaconInviteDecision::Hold(BeaconInviteHoldReason::OnCooldown);
    }
    // The window: after the lead time opens, before the show plays. A code
    // handed out for a show that already happened is a reminder about nothing.
    let window_hours = i64::from(policy.invite_lead_days) * 24;
    if snapshot.hours_until_event <= 0 {
        return BeaconInviteDecision::Hold(BeaconInviteHoldReason::WindowClosed);
    }
    if snapshot.hours_until_event > window_hours {
        return BeaconInviteDecision::Hold(BeaconInviteHoldReason::NotDue);
    }
    // Warmth is the confidence: a long-standing partner's yes means more than
    // a fresh contact's, and the authority ladder reads it as such.
    let confidence = Confidence::saturating_from_basis_points(
        u16::try_from(u32::from(snapshot.relationship_score).saturating_mul(100))
            .unwrap_or(u16::MAX),
    );
    BeaconInviteDecision::Request {
        requested_count: policy.max_invites_per_batch,
        confidence,
    }
}

#[cfg(test)]
mod beacon_invite_tests {
    use super::*;

    fn snapshot(relationship_score: u16, hours_until_event: i64) -> BeaconInviteSnapshot {
        BeaconInviteSnapshot {
            beacon_id: BeaconId::new(),
            beacon_version: 1,
            event_id: EventId::new(),
            kind: BeaconKind::Venue,
            active: true,
            verified: true,
            accepts_outreach: true,
            do_not_contact: false,
            relationship_score,
            hours_until_event,
            hours_since_last_invite_batch: None,
        }
    }

    #[test]
    fn a_warm_partner_inside_the_window_is_asked() {
        let decision =
            evaluate_beacon_invite_batch(snapshot(80, 24 * 14), BeaconInvitePolicy::default());
        let BeaconInviteDecision::Request {
            requested_count,
            confidence,
        } = decision
        else {
            panic!("expected a request, got {decision:?}");
        };
        assert_eq!(
            requested_count,
            BeaconInvitePolicy::default().max_invites_per_batch
        );
        assert!(confidence.basis_points() >= 7_000);
    }

    #[test]
    fn the_ask_is_never_made_to_a_stranger_or_a_refusal() {
        let policy = BeaconInvitePolicy::default();
        for mut cold in [snapshot(30, 24 * 10), snapshot(80, 24 * 10)] {
            if cold.relationship_score >= policy.minimum_relationship_score {
                continue;
            }
            assert_eq!(
                evaluate_beacon_invite_batch(cold, policy),
                BeaconInviteDecision::Hold(BeaconInviteHoldReason::LowWarmth)
            );
            let _ = &mut cold;
            break;
        }
        let mut refused = snapshot(80, 24 * 10);
        refused.do_not_contact = true;
        assert_eq!(
            evaluate_beacon_invite_batch(refused, policy),
            BeaconInviteDecision::Hold(BeaconInviteHoldReason::Ineligible)
        );
    }

    #[test]
    fn the_window_has_both_edges() {
        let policy = BeaconInvitePolicy::default();
        // Too far out: plans move.
        assert_eq!(
            evaluate_beacon_invite_batch(snapshot(80, 24 * 40), policy),
            BeaconInviteDecision::Hold(BeaconInviteHoldReason::NotDue)
        );
        // Already played: a reminder about nothing.
        assert_eq!(
            evaluate_beacon_invite_batch(snapshot(80, -4), policy),
            BeaconInviteDecision::Hold(BeaconInviteHoldReason::WindowClosed)
        );
    }

    #[test]
    fn one_batch_per_cooldown_even_for_the_best_partner() {
        let policy = BeaconInvitePolicy::default();
        let mut recent = snapshot(90, 24 * 10);
        recent.hours_since_last_invite_batch = Some(policy.invite_cooldown_days * 24 - 12);
        assert_eq!(
            evaluate_beacon_invite_batch(recent, policy),
            BeaconInviteDecision::Hold(BeaconInviteHoldReason::OnCooldown)
        );
        // And exactly past it, the same partner may be asked again.
        let mut aged = snapshot(90, 24 * 10);
        aged.hours_since_last_invite_batch = Some(policy.invite_cooldown_days * 24 + 1);
        assert!(matches!(
            evaluate_beacon_invite_batch(aged, policy),
            BeaconInviteDecision::Request { .. }
        ));
    }
}
