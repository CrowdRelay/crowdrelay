//! Deterministic attendance / demand loop for one live event.
//!
//! Professional promotion is not one blast. It is a sequence of distinct,
//! measurable levers: distribution hygiene, partner amplification, fan-to-fan
//! referrals, social proof, high-intent conversion and merch offers around the
//! show. This domain decides *which lever is due* from first-party facts. It
//! never sends mail, discovers contacts or asks an LLM to make a business
//! decision.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{EventId, autonomy::Confidence};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ShowGrowthHistory {
    pub free_listing_sweep_requested: bool,
    pub audience_capture_setup_requested: bool,
    pub partner_cross_promo_requested: bool,
    pub grassroots_scene_relay_requested: bool,
    pub fan_ambassadors_requested: bool,
    pub social_proof_relay_requested: bool,
    pub free_fan_channel_push_requested: bool,
    pub merch_buyer_offer_requested: bool,
    pub high_intent_last_mile_requested: bool,
    pub post_show_merch_requested: bool,
    pub post_show_follow_ask_requested: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ShowGrowthSnapshot {
    pub event_id: EventId,
    pub published: bool,
    pub communication_enabled: bool,
    pub starts_at: OffsetDateTime,
    /// Zero means the venue/sale capacity is unknown; pace checks then stay off.
    pub capacity: u32,
    pub paid_tickets: u32,
    pub paid_buyers: u32,
    pub paid_tickets_last_7d: u32,
    pub interested_fans: u32,
    pub city_signal_fans: u32,
    pub qualified_referrers_in_city: u32,
    pub beacon_partners: u16,
    pub attendees: u32,
    pub history: ShowGrowthHistory,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ShowGrowthPolicy {
    pub lookahead_days: u32,
    pub free_listing_lead_days: u32,
    pub audience_capture_lead_days: u32,
    pub partner_cross_promo_lead_days: u32,
    pub grassroots_scene_relay_lead_days: u32,
    pub fan_ambassador_lead_days: u32,
    pub social_proof_lead_days: u32,
    pub free_fan_channel_push_lead_days: u32,
    pub merch_preorder_lead_days: u32,
    pub last_mile_lead_days: u32,
    pub post_show_merch_hours: u32,
    /// How long after the show the follow ask stays worth sending. Longer than
    /// the merch window because it asks for something free, and the memory of a
    /// good night outlasts the impulse to buy a shirt.
    pub post_show_follow_ask_hours: u32,
    pub minimum_city_signal_fans: u32,
    pub minimum_referrers_for_ambassador_push: u32,
    pub minimum_paid_buyers_for_merch_offer: u32,
    pub minimum_unconverted_interest: u32,
    pub minimum_attendees_for_post_show_merch: u32,
    pub minimum_attendees_for_follow_ask: u32,
    /// Pace floor at <= 28 days before show.
    pub target_sold_28d_basis_points: u16,
    /// Pace floor at <= 14 days before show.
    pub target_sold_14d_basis_points: u16,
    /// Pace floor at <= 7 days before show.
    pub target_sold_7d_basis_points: u16,
}

impl Default for ShowGrowthPolicy {
    fn default() -> Self {
        Self {
            lookahead_days: 60,
            free_listing_lead_days: 56,
            audience_capture_lead_days: 52,
            partner_cross_promo_lead_days: 49,
            grassroots_scene_relay_lead_days: 42,
            fan_ambassador_lead_days: 35,
            social_proof_lead_days: 21,
            free_fan_channel_push_lead_days: 18,
            merch_preorder_lead_days: 14,
            last_mile_lead_days: 10,
            post_show_merch_hours: 36,
            post_show_follow_ask_hours: 72,
            minimum_city_signal_fans: 5,
            minimum_referrers_for_ambassador_push: 1,
            minimum_paid_buyers_for_merch_offer: 2,
            minimum_unconverted_interest: 3,
            minimum_attendees_for_post_show_merch: 3,
            minimum_attendees_for_follow_ask: 1,
            target_sold_28d_basis_points: 1_500,
            target_sold_14d_basis_points: 3_000,
            target_sold_7d_basis_points: 4_500,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShowGrowthLever {
    FreeListingSweep,
    AudienceCaptureSetup,
    PartnerCrossPromo,
    GrassrootsSceneRelay,
    FanAmbassadors,
    SocialProofRelay,
    FreeFanChannelPush,
    #[serde(alias = "merch_preorder_pickup")]
    MerchBuyerOffer,
    HighIntentLastMile,
    PostShowMerchFollowUp,
    /// Ask the people who were actually in the room to follow and track the
    /// band, so the next show finds them without anybody paying for reach.
    PostShowFollowAsk,
}

impl ShowGrowthLever {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FreeListingSweep => "free_listing_sweep",
            Self::AudienceCaptureSetup => "audience_capture_setup",
            Self::PartnerCrossPromo => "partner_cross_promo",
            Self::GrassrootsSceneRelay => "grassroots_scene_relay",
            Self::FanAmbassadors => "fan_ambassadors",
            Self::SocialProofRelay => "social_proof_relay",
            Self::FreeFanChannelPush => "free_fan_channel_push",
            Self::MerchBuyerOffer => "merch_buyer_offer",
            Self::HighIntentLastMile => "high_intent_last_mile",
            Self::PostShowMerchFollowUp => "post_show_merch_follow_up",
            Self::PostShowFollowAsk => "post_show_follow_ask",
        }
    }

    #[must_use]
    pub const fn template_key(self) -> &'static str {
        match self {
            Self::FreeListingSweep => "show.growth.free_listings.v1",
            Self::AudienceCaptureSetup => "show.growth.audience_capture.v1",
            Self::PartnerCrossPromo => "show.growth.partner_cross_promo.v1",
            Self::GrassrootsSceneRelay => "show.growth.grassroots_scene_relay.v1",
            Self::FanAmbassadors => "show.growth.fan_ambassadors.v1",
            Self::SocialProofRelay => "show.growth.social_proof.v1",
            Self::FreeFanChannelPush => "show.growth.free_fan_push.v1",
            Self::MerchBuyerOffer => "show.growth.merch_buyer_offer.v1",
            Self::HighIntentLastMile => "show.growth.high_intent.v1",
            Self::PostShowMerchFollowUp => "show.growth.post_show_merch.v1",
            Self::PostShowFollowAsk => "show.growth.post_show_follow_ask.v1",
        }
    }

    #[must_use]
    pub const fn is_first_party_campaign(self) -> bool {
        matches!(
            self,
            Self::FanAmbassadors
                | Self::FreeFanChannelPush
                | Self::MerchBuyerOffer
                | Self::HighIntentLastMile
                | Self::PostShowMerchFollowUp
                | Self::PostShowFollowAsk
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShowGrowthDecision {
    Hold(ShowGrowthHoldReason),
    Request {
        lever: ShowGrowthLever,
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShowGrowthHoldReason {
    InvalidPolicy,
    InvalidSnapshot,
    Unpublished,
    CommunicationDisabled,
    TooEarly,
    NotDue,
}

#[must_use]
pub fn evaluate_show_growth(
    snapshot: ShowGrowthSnapshot,
    policy: ShowGrowthPolicy,
    now: OffsetDateTime,
) -> ShowGrowthDecision {
    if !valid_policy(policy) {
        return ShowGrowthDecision::Hold(ShowGrowthHoldReason::InvalidPolicy);
    }
    if snapshot.paid_tickets > snapshot.capacity && snapshot.capacity > 0 {
        return ShowGrowthDecision::Hold(ShowGrowthHoldReason::InvalidSnapshot);
    }
    if !snapshot.published {
        return ShowGrowthDecision::Hold(ShowGrowthHoldReason::Unpublished);
    }
    let until = snapshot.starts_at - now;
    if until.is_negative() || until.is_zero() {
        let since_show = now - snapshot.starts_at;
        if since_show <= Duration::hours(i64::from(policy.post_show_merch_hours))
            && !snapshot.history.post_show_merch_requested
            && snapshot.communication_enabled
            && snapshot.attendees >= policy.minimum_attendees_for_post_show_merch
        {
            return request(ShowGrowthLever::PostShowMerchFollowUp, 9_200);
        }
        // The people who were in the room are the warmest audience the band
        // will ever have, and asking them to follow costs nothing. It runs
        // after the merch window rather than beside it, so nobody gets two
        // messages about the same night, and it stays open longer because a
        // free ask does not go stale the way an offer does.
        if since_show <= Duration::hours(i64::from(policy.post_show_follow_ask_hours))
            && !snapshot.history.post_show_follow_ask_requested
            && snapshot.communication_enabled
            && snapshot.attendees >= policy.minimum_attendees_for_follow_ask
        {
            return request(ShowGrowthLever::PostShowFollowAsk, 9_400);
        }
        return ShowGrowthDecision::Hold(ShowGrowthHoldReason::NotDue);
    }

    let days = until.whole_days();
    if days > i64::from(policy.lookahead_days) {
        return ShowGrowthDecision::Hold(ShowGrowthHoldReason::TooEarly);
    }

    // Distribution hygiene first: be present where local people actually look.
    if days <= i64::from(policy.free_listing_lead_days)
        && !snapshot.history.free_listing_sweep_requested
    {
        return request(ShowGrowthLever::FreeListingSweep, 9_500);
    }

    // Capture free intent before amplification starts. Owned VIRYA surfaces keep
    // Signal as the primary first-party relationship, while third-party live
    // discovery tools may collect their own followers/RSVPs without importing
    // Signal contacts or weakening consent boundaries.
    if days <= i64::from(policy.audience_capture_lead_days)
        && !snapshot.history.audience_capture_setup_requested
    {
        return request(ShowGrowthLever::AudienceCaptureSetup, 9_450);
    }

    // A promoter borrows audiences. Ask venue, bill partners, scene communities
    // and already-known Beacons to relay the same ticket link instead of making
    // the band carry all discovery alone.
    if days <= i64::from(policy.partner_cross_promo_lead_days)
        && !snapshot.history.partner_cross_promo_requested
    {
        return request(ShowGrowthLever::PartnerCrossPromo, 9_400);
    }

    // Activate the real local scene graph after broad partner outreach but before
    // asking fans to relay. This is deliberately relationship-first: verified
    // record stores, rehearsal rooms, photographers, alt businesses, student
    // media and moderated communities get one useful ask or one consented warm
    // introduction, never a cold mass blast.
    if days <= i64::from(policy.grassroots_scene_relay_lead_days)
        && !snapshot.history.grassroots_scene_relay_requested
    {
        return request(ShowGrowthLever::GrassrootsSceneRelay, 9_350);
    }

    // Turn the strongest local first-party fans into a small street team. This
    // uses existing referral identities; no mass cold messaging is introduced.
    if days <= i64::from(policy.fan_ambassador_lead_days)
        && !snapshot.history.fan_ambassadors_requested
        && snapshot.communication_enabled
        && snapshot.city_signal_fans >= policy.minimum_city_signal_fans
        && snapshot.qualified_referrers_in_city >= policy.minimum_referrers_for_ambassador_push
    {
        return request(ShowGrowthLever::FanAmbassadors, 9_100);
    }

    let behind = behind_sales_pace(snapshot, policy, days);
    if days <= i64::from(policy.social_proof_lead_days)
        && !snapshot.history.social_proof_relay_requested
        && behind
    {
        return request(ShowGrowthLever::SocialProofRelay, 8_900);
    }

    // Use free provider-native follower surfaces while the show still has time
    // to convert: location/RSVP-targeted Bandsintown messages/email and a
    // Spotify profile feature. These remain free-only and provider-confirmed;
    // paid Boost/Promoted Campaigns are outside this authority.
    if days <= i64::from(policy.free_fan_channel_push_lead_days)
        && !snapshot.history.free_fan_channel_push_requested
    {
        return request(ShowGrowthLever::FreeFanChannelPush, 9_250);
    }

    // Ticket buyers are the highest-intent merch audience. Give them a useful
    // pre-show merch offer while intent is high. Fulfilment stays owned by the
    // commerce surface: never promise venue pickup unless checkout supports it.
    if days <= i64::from(policy.merch_preorder_lead_days)
        && !snapshot.history.merch_buyer_offer_requested
        && snapshot.communication_enabled
        && snapshot.paid_buyers >= policy.minimum_paid_buyers_for_merch_offer
    {
        return request(ShowGrowthLever::MerchBuyerOffer, 9_300);
    }

    let unconverted = snapshot
        .interested_fans
        .saturating_sub(snapshot.paid_buyers);
    if days <= i64::from(policy.last_mile_lead_days)
        && !snapshot.history.high_intent_last_mile_requested
        && snapshot.communication_enabled
        && behind
        && unconverted >= policy.minimum_unconverted_interest
    {
        return request(ShowGrowthLever::HighIntentLastMile, 9_500);
    }

    ShowGrowthDecision::Hold(ShowGrowthHoldReason::NotDue)
}

fn request(lever: ShowGrowthLever, basis_points: u16) -> ShowGrowthDecision {
    ShowGrowthDecision::Request {
        lever,
        confidence: Confidence::saturating_from_basis_points(basis_points),
    }
}

#[must_use]
pub fn sold_basis_points(snapshot: ShowGrowthSnapshot) -> Option<u16> {
    if snapshot.capacity == 0 {
        return None;
    }
    let value =
        u64::from(snapshot.paid_tickets).saturating_mul(10_000) / u64::from(snapshot.capacity);
    Some(u16::try_from(value.min(10_000)).unwrap_or(10_000))
}

fn behind_sales_pace(snapshot: ShowGrowthSnapshot, policy: ShowGrowthPolicy, days: i64) -> bool {
    let Some(sold) = sold_basis_points(snapshot) else {
        // Unknown venue capacity should not block free organic levers. A flat
        // week close to the show is enough evidence that another push is useful.
        return days <= 14 && snapshot.paid_tickets_last_7d == 0;
    };
    let target = if days <= 7 {
        policy.target_sold_7d_basis_points
    } else if days <= 14 {
        policy.target_sold_14d_basis_points
    } else if days <= 28 {
        policy.target_sold_28d_basis_points
    } else {
        0
    };
    target > 0 && sold < target
}

const fn valid_policy(policy: ShowGrowthPolicy) -> bool {
    policy.lookahead_days >= policy.free_listing_lead_days
        && policy.free_listing_lead_days >= policy.audience_capture_lead_days
        && policy.audience_capture_lead_days >= policy.partner_cross_promo_lead_days
        && policy.partner_cross_promo_lead_days >= policy.grassroots_scene_relay_lead_days
        && policy.grassroots_scene_relay_lead_days >= policy.fan_ambassador_lead_days
        && policy.fan_ambassador_lead_days >= policy.social_proof_lead_days
        && policy.social_proof_lead_days >= policy.free_fan_channel_push_lead_days
        && policy.free_fan_channel_push_lead_days >= policy.merch_preorder_lead_days
        && policy.merch_preorder_lead_days >= policy.last_mile_lead_days
        && policy.last_mile_lead_days > 0
        && policy.post_show_merch_hours > 0
        && policy.post_show_follow_ask_hours >= policy.post_show_merch_hours
        && policy.minimum_attendees_for_follow_ask > 0
        && policy.minimum_city_signal_fans > 0
        && policy.minimum_paid_buyers_for_merch_offer > 0
        && policy.minimum_unconverted_interest > 0
        && policy.minimum_attendees_for_post_show_merch > 0
        && policy.target_sold_28d_basis_points <= policy.target_sold_14d_basis_points
        && policy.target_sold_14d_basis_points <= policy.target_sold_7d_basis_points
        && policy.target_sold_7d_basis_points <= 10_000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    fn snapshot(days: i64) -> ShowGrowthSnapshot {
        ShowGrowthSnapshot {
            event_id: EventId::new(),
            published: true,
            communication_enabled: true,
            starts_at: now() + Duration::days(days),
            capacity: 100,
            paid_tickets: 8,
            paid_buyers: 6,
            paid_tickets_last_7d: 2,
            interested_fans: 30,
            city_signal_fans: 20,
            qualified_referrers_in_city: 4,
            beacon_partners: 0,
            attendees: 0,
            history: ShowGrowthHistory::default(),
        }
    }

    #[test]
    fn starts_with_free_distribution_hygiene() {
        let decision = evaluate_show_growth(snapshot(50), ShowGrowthPolicy::default(), now());
        assert!(matches!(
            decision,
            ShowGrowthDecision::Request {
                lever: ShowGrowthLever::FreeListingSweep,
                ..
            }
        ));
    }

    #[test]
    fn cross_promo_follows_listing_sweep() {
        let mut data = snapshot(40);
        data.history.free_listing_sweep_requested = true;
        data.history.audience_capture_setup_requested = true;
        let decision = evaluate_show_growth(data, ShowGrowthPolicy::default(), now());
        assert!(matches!(
            decision,
            ShowGrowthDecision::Request {
                lever: ShowGrowthLever::PartnerCrossPromo,
                ..
            }
        ));
    }

    #[test]
    fn grassroots_scene_relay_follows_partner_cross_promo() {
        let mut data = snapshot(40);
        data.history.free_listing_sweep_requested = true;
        data.history.audience_capture_setup_requested = true;
        data.history.partner_cross_promo_requested = true;
        assert!(matches!(
            evaluate_show_growth(data, ShowGrowthPolicy::default(), now()),
            ShowGrowthDecision::Request {
                lever: ShowGrowthLever::GrassrootsSceneRelay,
                ..
            }
        ));
    }

    #[test]
    fn ambassadors_require_real_local_referral_strength() {
        let mut data = snapshot(30);
        data.history.free_listing_sweep_requested = true;
        data.history.audience_capture_setup_requested = true;
        data.history.partner_cross_promo_requested = true;
        data.history.grassroots_scene_relay_requested = true;
        data.qualified_referrers_in_city = 0;
        assert_eq!(
            evaluate_show_growth(data, ShowGrowthPolicy::default(), now()),
            ShowGrowthDecision::Hold(ShowGrowthHoldReason::NotDue)
        );
        data.qualified_referrers_in_city = 2;
        assert!(matches!(
            evaluate_show_growth(data, ShowGrowthPolicy::default(), now()),
            ShowGrowthDecision::Request {
                lever: ShowGrowthLever::FanAmbassadors,
                ..
            }
        ));
    }

    #[test]
    fn slow_sales_trigger_social_proof_then_high_intent() {
        let mut data = snapshot(8);
        data.history = ShowGrowthHistory {
            free_listing_sweep_requested: true,
            audience_capture_setup_requested: true,
            partner_cross_promo_requested: true,
            grassroots_scene_relay_requested: true,
            fan_ambassadors_requested: true,
            ..ShowGrowthHistory::default()
        };
        data.paid_tickets = 20;
        assert!(matches!(
            evaluate_show_growth(data, ShowGrowthPolicy::default(), now()),
            ShowGrowthDecision::Request {
                lever: ShowGrowthLever::SocialProofRelay,
                ..
            }
        ));
        data.history.social_proof_relay_requested = true;
        data.history.free_fan_channel_push_requested = true;
        data.history.merch_buyer_offer_requested = true;
        assert!(matches!(
            evaluate_show_growth(data, ShowGrowthPolicy::default(), now()),
            ShowGrowthDecision::Request {
                lever: ShowGrowthLever::HighIntentLastMile,
                ..
            }
        ));
    }

    #[test]
    fn ticket_buyers_get_a_merch_offer() {
        let mut data = snapshot(12);
        data.history = ShowGrowthHistory {
            free_listing_sweep_requested: true,
            audience_capture_setup_requested: true,
            partner_cross_promo_requested: true,
            grassroots_scene_relay_requested: true,
            fan_ambassadors_requested: true,
            social_proof_relay_requested: true,
            free_fan_channel_push_requested: true,
            ..ShowGrowthHistory::default()
        };
        let decision = evaluate_show_growth(data, ShowGrowthPolicy::default(), now());
        assert!(matches!(
            decision,
            ShowGrowthDecision::Request {
                lever: ShowGrowthLever::MerchBuyerOffer,
                ..
            }
        ));
    }

    #[test]
    fn audience_capture_follows_listing_hygiene_before_partner_amplification() {
        let mut data = snapshot(51);
        data.history.free_listing_sweep_requested = true;
        assert!(matches!(
            evaluate_show_growth(data, ShowGrowthPolicy::default(), now()),
            ShowGrowthDecision::Request {
                lever: ShowGrowthLever::AudienceCaptureSetup,
                ..
            }
        ));
    }

    #[test]
    fn free_provider_fan_push_is_due_after_social_proof_wave() {
        let mut data = snapshot(17);
        data.history = ShowGrowthHistory {
            free_listing_sweep_requested: true,
            audience_capture_setup_requested: true,
            partner_cross_promo_requested: true,
            grassroots_scene_relay_requested: true,
            fan_ambassadors_requested: true,
            social_proof_relay_requested: true,
            ..ShowGrowthHistory::default()
        };
        assert!(matches!(
            evaluate_show_growth(data, ShowGrowthPolicy::default(), now()),
            ShowGrowthDecision::Request {
                lever: ShowGrowthLever::FreeFanChannelPush,
                ..
            }
        ));
    }

    #[test]
    fn free_fan_channel_push_is_first_party_executable() {
        assert!(ShowGrowthLever::FreeFanChannelPush.is_first_party_campaign());
    }

    #[test]
    fn external_free_levers_still_run_when_first_party_campaigns_are_disabled() {
        let mut data = snapshot(50);
        data.communication_enabled = false;
        assert!(matches!(
            evaluate_show_growth(data, ShowGrowthPolicy::default(), now()),
            ShowGrowthDecision::Request {
                lever: ShowGrowthLever::FreeListingSweep,
                ..
            }
        ));
    }

    #[test]
    fn attendees_get_one_short_post_show_merch_window() {
        let mut data = snapshot(-1);
        data.starts_at = now() - Duration::hours(18);
        data.attendees = 14;
        let decision = evaluate_show_growth(data, ShowGrowthPolicy::default(), now());
        assert!(matches!(
            decision,
            ShowGrowthDecision::Request {
                lever: ShowGrowthLever::PostShowMerchFollowUp,
                ..
            }
        ));
    }

    #[test]
    fn the_room_is_asked_to_follow_once_the_merch_window_closes() {
        // The warmest audience the band will ever have, asked for something
        // free. This is the lever that actually moves followers and trackers.
        let mut data = snapshot(-1);
        data.starts_at = now() - Duration::hours(48);
        data.attendees = 40;
        data.history.post_show_merch_requested = true;
        assert!(matches!(
            evaluate_show_growth(data, ShowGrowthPolicy::default(), now()),
            ShowGrowthDecision::Request {
                lever: ShowGrowthLever::PostShowFollowAsk,
                ..
            }
        ));
    }

    #[test]
    fn the_merch_window_still_wins_while_it_is_open() {
        // Two messages about the same night is how an audience unsubscribes.
        let mut data = snapshot(-1);
        data.starts_at = now() - Duration::hours(6);
        data.attendees = 40;
        assert!(matches!(
            evaluate_show_growth(data, ShowGrowthPolicy::default(), now()),
            ShowGrowthDecision::Request {
                lever: ShowGrowthLever::PostShowMerchFollowUp,
                ..
            }
        ));
    }

    #[test]
    fn the_follow_ask_is_made_once_and_then_stops() {
        let mut data = snapshot(-1);
        data.starts_at = now() - Duration::hours(48);
        data.attendees = 40;
        data.history.post_show_merch_requested = true;
        data.history.post_show_follow_ask_requested = true;
        assert_eq!(
            evaluate_show_growth(data, ShowGrowthPolicy::default(), now()),
            ShowGrowthDecision::Hold(ShowGrowthHoldReason::NotDue)
        );
    }

    #[test]
    fn a_show_nobody_came_to_is_asked_for_nothing() {
        let mut data = snapshot(-1);
        data.starts_at = now() - Duration::hours(48);
        data.attendees = 0;
        data.history.post_show_merch_requested = true;
        assert_eq!(
            evaluate_show_growth(data, ShowGrowthPolicy::default(), now()),
            ShowGrowthDecision::Hold(ShowGrowthHoldReason::NotDue)
        );
    }

    #[test]
    fn the_follow_window_may_not_close_before_the_merch_window() {
        // Otherwise the merch message runs and the free ask silently never can.
        let policy = ShowGrowthPolicy {
            post_show_follow_ask_hours: 1,
            ..ShowGrowthPolicy::default()
        };
        assert_eq!(
            evaluate_show_growth(snapshot(10), policy, now()),
            ShowGrowthDecision::Hold(ShowGrowthHoldReason::InvalidPolicy)
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShowLifecyclePhase {
    Planning,
    Amplify,
    Convert,
    Ready,
    Live,
    Afterglow,
    Review,
    Complete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ShowLifecycleView {
    pub phase: ShowLifecyclePhase,
    pub next_milestone: Option<&'static str>,
    pub next_milestone_at: Option<OffsetDateTime>,
}

/// Human/operator lifecycle derived from the exact same timing policy used by
/// Show Growth. It is presentation state only: it never schedules a second job.
#[must_use]
pub fn show_lifecycle(
    starts_at: OffsetDateTime,
    now: OffsetDateTime,
    policy: ShowGrowthPolicy,
) -> ShowLifecycleView {
    let afterglow_end = starts_at + Duration::hours(i64::from(policy.post_show_merch_hours));
    let live_end = starts_at + Duration::hours(6);
    let review_end = starts_at + Duration::days(7);
    let phase = if now >= review_end {
        ShowLifecyclePhase::Complete
    } else if now >= afterglow_end {
        ShowLifecyclePhase::Review
    } else if now >= live_end {
        ShowLifecyclePhase::Afterglow
    } else if now >= starts_at {
        ShowLifecyclePhase::Live
    } else {
        let days = (starts_at - now).whole_days();
        if days <= i64::from(policy.last_mile_lead_days) {
            ShowLifecyclePhase::Ready
        } else if days <= i64::from(policy.free_fan_channel_push_lead_days) {
            ShowLifecyclePhase::Convert
        } else if days <= i64::from(policy.partner_cross_promo_lead_days) {
            ShowLifecyclePhase::Amplify
        } else {
            ShowLifecyclePhase::Planning
        }
    };

    let before = |days: u32| starts_at - Duration::days(i64::from(days));
    let milestones = [
        ("free_listing_sweep", before(policy.free_listing_lead_days)),
        (
            "audience_capture",
            before(policy.audience_capture_lead_days),
        ),
        (
            "partner_cross_promo",
            before(policy.partner_cross_promo_lead_days),
        ),
        (
            "grassroots_scene_relay",
            before(policy.grassroots_scene_relay_lead_days),
        ),
        ("fan_ambassadors", before(policy.fan_ambassador_lead_days)),
        ("social_proof", before(policy.social_proof_lead_days)),
        (
            "fan_channel_push",
            before(policy.free_fan_channel_push_lead_days),
        ),
        ("merch_offer", before(policy.merch_preorder_lead_days)),
        ("last_mile", before(policy.last_mile_lead_days)),
        ("show_start", starts_at),
        ("afterglow", live_end),
        ("post_show_merch", afterglow_end),
        (
            "post_show_follow_ask",
            starts_at + Duration::hours(i64::from(policy.post_show_follow_ask_hours)),
        ),
        ("review_complete", review_end),
    ];
    let next = milestones.into_iter().find(|(_, at)| *at > now);
    ShowLifecycleView {
        phase,
        next_milestone: next.map(|(name, _)| name),
        next_milestone_at: next.map(|(_, at)| at),
    }
}
