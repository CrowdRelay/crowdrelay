//! Relationship-aware outreach bounded context.
//!
//! External intelligence may create an opportunity, but only a verified,
//! operator-owned target can ever be selected. The domain has no email address
//! and cannot send anything; it only decides whether an initial touch or bounded
//! follow-up is appropriate.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{
    OutreachOpportunityId, OutreachTargetId, autonomy::Confidence, free_reach::FreeReachPolicy,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutreachTargetKind {
    Playlist,
    Radio,
    Press,
    Creator,
    SupportSlot,
    Endorsement,
    MediaPatronage,
    Agent,
    Label,
}

impl OutreachTargetKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Playlist => "playlist",
            Self::Radio => "radio",
            Self::Press => "press",
            Self::Creator => "creator",
            Self::SupportSlot => "support_slot",
            Self::Endorsement => "endorsement",
            Self::MediaPatronage => "media_patronage",
            Self::Agent => "agent",
            Self::Label => "label",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "playlist" => Some(Self::Playlist),
            "radio" => Some(Self::Radio),
            "press" => Some(Self::Press),
            "creator" => Some(Self::Creator),
            "support_slot" => Some(Self::SupportSlot),
            "endorsement" => Some(Self::Endorsement),
            "media_patronage" => Some(Self::MediaPatronage),
            "agent" => Some(Self::Agent),
            "label" => Some(Self::Label),
            _ => None,
        }
    }

    #[must_use]
    pub const fn all() -> [Self; 9] {
        [
            Self::Playlist,
            Self::Radio,
            Self::Press,
            Self::Creator,
            Self::SupportSlot,
            Self::Endorsement,
            Self::MediaPatronage,
            Self::Agent,
            Self::Label,
        ]
    }

    /// Whether this is a representation contact — a booking agent or a
    /// label the band approaches for its career rather than a channel it
    /// pitches a release to. Representation kinds carry the consent basis
    /// rule: `accepts_outreach` may only be true when a basis was stated,
    /// which `review_approach` and the schema CHECK enforce together.
    #[must_use]
    pub const fn is_representation(self) -> bool {
        matches!(self, Self::Agent | Self::Label)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutreachReplyDisposition {
    None,
    /// A reply exists, but no semantic classification was required.
    Received,
    Positive,
    Declined,
    DoNotContact,
}

impl OutreachReplyDisposition {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Received => "received",
            Self::Positive => "positive",
            Self::Declined => "declined",
            Self::DoNotContact => "do_not_contact",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct OutreachSnapshot {
    pub opportunity_id: OutreachOpportunityId,
    pub target_id: OutreachTargetId,
    pub target_kind: OutreachTargetKind,
    pub target_version: i64,
    pub active: bool,
    pub verified: bool,
    pub accepts_outreach: bool,
    pub relevance_basis_points: u16,
    pub evidence_confidence: Confidence,
    #[serde(with = "time::serde::rfc3339")]
    pub observed_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    /// Latest outbound touch for this specific opportunity.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_outreach_at: Option<OffsetDateTime>,
    /// Latest outbound touch to this relationship across any opportunity.
    #[serde(with = "time::serde::rfc3339::option")]
    pub target_last_outreach_at: Option<OffsetDateTime>,
    pub followup_count: u16,
    /// Every outbound touch this relationship has ever received, across all
    /// opportunities. `followup_count` is scoped to one opportunity and resets
    /// with it; this does not.
    pub lifetime_outbound: u16,
    /// Whether this relationship has ever answered, on any opportunity.
    ///
    /// Silence and a conversation are different states. Without this, a contact
    /// who replied last year looks identical to one who has never responded,
    /// because the reply belongs to an opportunity that has since expired.
    pub target_ever_replied: bool,
    pub last_reply: OutreachReplyDisposition,
    pub in_flight: bool,
    /// The opportunity may only be pitched inside a wave. Catalogue pitches
    /// are evergreen and plentiful — 196 contacts for one album in
    /// production — so outside a wave they would arrive as one approval card
    /// each, up to the daily quota, and lapse unread. A wave is how a person
    /// reads a dozen of them at once.
    pub wave_only: bool,
    /// The opportunity is a follow-up on a thread the act started by hand.
    ///
    /// The imported messages carry no opportunity id, so the per-opportunity
    /// fields stay empty and the relationship-level window takes over: one
    /// nudge, sent inside `thread_followup_after_days..thread_followup_window_days`
    /// of the last handwritten touch, and never again.
    pub thread_followup: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
// Stored configs predate the wave knobs, and a policy row that fails to parse
// takes the whole context down rather than one field.
#[serde(default)]
pub struct OutreachPolicy {
    pub minimum_relevance_basis_points: u16,
    pub minimum_relationship_confidence_basis_points: u16,
    pub initial_cooldown_days: u32,
    pub followup_after_days: u32,
    pub declined_cooldown_days: u32,
    pub maximum_followups: u16,
    /// Total outbound touches a silent contact may ever receive.
    ///
    /// `maximum_followups` bounds one opportunity. Nothing bounded the
    /// relationship: a new opportunity for the same address starts at
    /// `Initial`, which resets the follow-up counter, so a contact who never
    /// replied kept receiving a fresh "first" pitch every cooldown for as long
    /// as opportunities kept being discovered. That is indistinguishable from
    /// spam to the person receiving it, and it is what happened.
    pub maximum_lifetime_contacts: u16,
    /// Days of silence before a hand-started thread earns its one nudge.
    /// Under it, the thread is the act's own to answer.
    pub thread_followup_after_days: u32,
    /// Days after which the thread is too old to reopen. The nudge says
    /// "following up on my message"; past the window that claim stops being
    /// one a stranger can still place.
    pub thread_followup_window_days: u32,
    /// How free-reach pitches are batched for approval. Nested here rather than
    /// given a context of its own because it is the same operator setting: how
    /// this workspace approaches people it does not know, and how much of that
    /// a human is asked to read at once.
    pub waves: FreeReachPolicy,
}

impl Default for OutreachPolicy {
    fn default() -> Self {
        Self {
            minimum_relevance_basis_points: 7_000,
            minimum_relationship_confidence_basis_points: 7_500,
            initial_cooldown_days: 90,
            followup_after_days: 5,
            declined_cooldown_days: 180,
            maximum_followups: 1,
            // One pitch and one follow-up. A third unanswered email to a
            // stranger is not persistence.
            maximum_lifetime_contacts: 2,
            thread_followup_after_days: 10,
            thread_followup_window_days: 60,
            waves: FreeReachPolicy::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutreachPhase {
    Initial,
    FollowUp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutreachDecision {
    Hold(OutreachHoldReason),
    Request {
        phase: OutreachPhase,
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutreachHoldReason {
    InvalidPolicy,
    InvalidSnapshot,
    IneligibleTarget,
    StaleOpportunity,
    LowRelevance,
    InFlight,
    AlreadyReplied,
    Cooldown,
    FollowUpNotDue,
    FollowUpLimit,
    /// This relationship has had every touch it is ever going to get without
    /// answering one. Unlike the other holds this one does not expire.
    ContactExhausted,
}

#[must_use]
pub fn evaluate_outreach(
    snapshot: OutreachSnapshot,
    policy: OutreachPolicy,
    now: OffsetDateTime,
) -> OutreachDecision {
    if !policy_is_valid(policy) {
        return OutreachDecision::Hold(OutreachHoldReason::InvalidPolicy);
    }
    if snapshot.observed_at > now
        || snapshot.expires_at <= snapshot.observed_at
        || snapshot.target_version <= 0
    {
        return OutreachDecision::Hold(OutreachHoldReason::InvalidSnapshot);
    }
    if !snapshot.active || !snapshot.verified || !snapshot.accepts_outreach {
        return OutreachDecision::Hold(OutreachHoldReason::IneligibleTarget);
    }
    if snapshot.expires_at <= now {
        return OutreachDecision::Hold(OutreachHoldReason::StaleOpportunity);
    }
    if snapshot.relevance_basis_points < policy.minimum_relevance_basis_points
        || snapshot.evidence_confidence.basis_points()
            < policy.minimum_relationship_confidence_basis_points
    {
        return OutreachDecision::Hold(OutreachHoldReason::LowRelevance);
    }
    if snapshot.in_flight {
        return OutreachDecision::Hold(OutreachHoldReason::InFlight);
    }
    match snapshot.last_reply {
        OutreachReplyDisposition::Received
        | OutreachReplyDisposition::Positive
        | OutreachReplyDisposition::DoNotContact => {
            return OutreachDecision::Hold(OutreachHoldReason::AlreadyReplied);
        }
        OutreachReplyDisposition::Declined => {
            if snapshot.last_outreach_at.is_none_or(|at| {
                now - at < Duration::days(i64::from(policy.declined_cooldown_days))
            }) {
                return OutreachDecision::Hold(OutreachHoldReason::Cooldown);
            }
        }
        OutreachReplyDisposition::None => {}
    }

    // Silence is an answer. Checked before the phase split, because the spam
    // path ran through `Initial`: a fresh opportunity has no outreach of its
    // own, so it fell straight through to a brand-new first contact no matter
    // how many the person had already ignored.
    if !snapshot.target_ever_replied
        && snapshot.lifetime_outbound >= policy.maximum_lifetime_contacts
    {
        return OutreachDecision::Hold(OutreachHoldReason::ContactExhausted);
    }

    let Some(last_outreach) = snapshot.last_outreach_at else {
        // A thread the act started by hand skips the initial cooldown — the
        // whole point of the lane is that the relationship clock was already
        // running on a message the importer never linked. Its own rule is the
        // window: not before `thread_followup_after_days` of silence, and not
        // after `thread_followup_window_days` have made the thread stale.
        if snapshot.thread_followup {
            let Some(last) = snapshot.target_last_outreach_at else {
                return OutreachDecision::Hold(OutreachHoldReason::InvalidSnapshot);
            };
            if last > now {
                return OutreachDecision::Hold(OutreachHoldReason::InvalidSnapshot);
            }
            if now - last < Duration::days(i64::from(policy.thread_followup_after_days)) {
                return OutreachDecision::Hold(OutreachHoldReason::FollowUpNotDue);
            }
            if now - last >= Duration::days(i64::from(policy.thread_followup_window_days)) {
                return OutreachDecision::Hold(OutreachHoldReason::StaleOpportunity);
            }
            return request(OutreachPhase::FollowUp, snapshot);
        }
        if snapshot.target_last_outreach_at.is_some_and(|at| {
            at > now || now - at < Duration::days(i64::from(policy.initial_cooldown_days))
        }) {
            return OutreachDecision::Hold(OutreachHoldReason::Cooldown);
        }
        return request(OutreachPhase::Initial, snapshot);
    };
    if last_outreach > now {
        return OutreachDecision::Hold(OutreachHoldReason::InvalidSnapshot);
    }
    if snapshot.followup_count >= policy.maximum_followups {
        return OutreachDecision::Hold(OutreachHoldReason::FollowUpLimit);
    }
    if now - last_outreach < Duration::days(i64::from(policy.followup_after_days)) {
        return OutreachDecision::Hold(OutreachHoldReason::FollowUpNotDue);
    }
    request(OutreachPhase::FollowUp, snapshot)
}

fn request(phase: OutreachPhase, snapshot: OutreachSnapshot) -> OutreachDecision {
    let relevance_bonus = snapshot
        .relevance_basis_points
        .saturating_sub(7_000)
        .min(1_000);
    let confidence = snapshot
        .evidence_confidence
        .basis_points()
        .min(9_000)
        .saturating_add(relevance_bonus);
    OutreachDecision::Request {
        phase,
        confidence: Confidence::saturating_from_basis_points(confidence.min(10_000)),
    }
}

fn policy_is_valid(policy: OutreachPolicy) -> bool {
    (1..=10_000).contains(&policy.minimum_relevance_basis_points)
        && (1..=10_000).contains(&policy.minimum_relationship_confidence_basis_points)
        && policy.initial_cooldown_days > policy.followup_after_days
        && policy.declined_cooldown_days >= policy.initial_cooldown_days
        && policy.followup_after_days > 0
        && policy.maximum_followups <= 3
        && policy.maximum_lifetime_contacts >= 1
        && policy.maximum_lifetime_contacts <= 4
        && policy.thread_followup_after_days > 0
        && policy.thread_followup_after_days < policy.thread_followup_window_days
        && policy.thread_followup_window_days <= 365
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The antyradio case: a silent contact must not receive an endless
    /// sequence of "first" pitches.
    ///
    /// Every send went out as `Initial`, because each new opportunity carries
    /// no outreach of its own. `Initial` resets `followup_count`, so the
    /// follow-up cap never engaged and the only spacing was the initial
    /// cooldown. Bounding the relationship rather than the opportunity is what
    /// stops it.
    #[test]
    fn a_silent_contact_is_never_pitched_past_the_lifetime_cap() {
        let policy = OutreachPolicy::default();
        let mut snapshot = eligible();
        // A brand-new opportunity: nothing sent on it yet, so the old code
        // took the Initial branch regardless of history.
        snapshot.last_outreach_at = None;
        snapshot.target_last_outreach_at = None;
        snapshot.target_ever_replied = false;

        snapshot.lifetime_outbound = policy.maximum_lifetime_contacts - 1;
        assert!(
            matches!(
                evaluate_outreach(snapshot, policy, now()),
                OutreachDecision::Request { .. }
            ),
            "a contact below the lifetime cap may still be approached"
        );

        snapshot.lifetime_outbound = policy.maximum_lifetime_contacts;
        assert_eq!(
            evaluate_outreach(snapshot, policy, now()),
            OutreachDecision::Hold(OutreachHoldReason::ContactExhausted),
            "at the cap a new opportunity must not restart the sequence"
        );

        snapshot.lifetime_outbound = 50;
        assert_eq!(
            evaluate_outreach(snapshot, policy, now()),
            OutreachDecision::Hold(OutreachHoldReason::ContactExhausted),
            "and no amount of further opportunities may reopen it"
        );
    }

    /// The cap answers silence, not conversation. Someone who replied is in a
    /// relationship, and the other holds govern what happens next.
    #[test]
    fn a_contact_who_has_ever_replied_is_not_silence_capped() {
        let policy = OutreachPolicy::default();
        let mut snapshot = eligible();
        snapshot.last_outreach_at = None;
        snapshot.target_last_outreach_at = None;
        snapshot.lifetime_outbound = 50;
        snapshot.target_ever_replied = true;

        assert_ne!(
            evaluate_outreach(snapshot, policy, now()),
            OutreachDecision::Hold(OutreachHoldReason::ContactExhausted),
        );
    }

    /// Default policy is one pitch and one follow-up.
    #[test]
    fn the_default_lifetime_cap_is_two_touches() {
        assert_eq!(OutreachPolicy::default().maximum_lifetime_contacts, 2);
        assert!(policy_is_valid(OutreachPolicy::default()));
        assert!(!policy_is_valid(OutreachPolicy {
            maximum_lifetime_contacts: 0,
            ..OutreachPolicy::default()
        }));
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }
    fn eligible() -> OutreachSnapshot {
        OutreachSnapshot {
            opportunity_id: OutreachOpportunityId::new(),
            target_id: OutreachTargetId::new(),
            target_kind: OutreachTargetKind::Radio,
            target_version: 1,
            active: true,
            verified: true,
            accepts_outreach: true,
            relevance_basis_points: 8_500,
            evidence_confidence: Confidence::saturating_from_basis_points(9_000),
            observed_at: now() - Duration::hours(1),
            expires_at: now() + Duration::days(14),
            last_outreach_at: None,
            target_last_outreach_at: None,
            followup_count: 0,
            lifetime_outbound: 0,
            target_ever_replied: false,
            last_reply: OutreachReplyDisposition::None,
            in_flight: false,
            wave_only: false,
            thread_followup: false,
        }
    }

    #[test]
    fn unverified_target_can_never_be_contacted() {
        let mut data = eligible();
        data.verified = false;
        assert_eq!(
            evaluate_outreach(data, OutreachPolicy::default(), now()),
            OutreachDecision::Hold(OutreachHoldReason::IneligibleTarget)
        );
    }
    #[test]
    fn one_bounded_followup_becomes_due() {
        let mut data = eligible();
        data.last_outreach_at = Some(now() - Duration::days(6));
        assert!(matches!(
            evaluate_outreach(data, OutreachPolicy::default(), now()),
            OutreachDecision::Request {
                phase: OutreachPhase::FollowUp,
                ..
            }
        ));
    }
    #[test]
    fn any_received_reply_stops_followups_without_needing_semantic_classification() {
        let mut data = eligible();
        data.last_outreach_at = Some(now() - Duration::days(6));
        data.last_reply = OutreachReplyDisposition::Received;
        assert_eq!(
            evaluate_outreach(data, OutreachPolicy::default(), now()),
            OutreachDecision::Hold(OutreachHoldReason::AlreadyReplied),
        );
    }

    #[test]
    fn a_new_opportunity_respects_relationship_level_initial_cooldown() {
        let mut snapshot = eligible();
        snapshot.target_last_outreach_at = Some(now() - Duration::days(10));
        assert_eq!(
            evaluate_outreach(snapshot, OutreachPolicy::default(), now()),
            OutreachDecision::Hold(OutreachHoldReason::Cooldown)
        );
    }

    /// The imported case: the act wrote by hand, the sheet carried the date
    /// and no opportunity, and the 90-day cooldown would hold the thread for
    /// ever. The thread lane replaces that cooldown with its own window.
    #[test]
    fn a_thread_followup_skips_the_initial_cooldown_inside_its_window() {
        let mut snapshot = eligible();
        snapshot.thread_followup = true;
        snapshot.wave_only = true;
        snapshot.last_outreach_at = None;
        snapshot.target_last_outreach_at = Some(now() - Duration::days(12));
        assert!(matches!(
            evaluate_outreach(snapshot, OutreachPolicy::default(), now()),
            OutreachDecision::Request {
                phase: OutreachPhase::FollowUp,
                ..
            }
        ));
    }

    /// Under ten days the thread is still the act's own to answer; past sixty
    /// the "following up on my message" claim is one nobody can place.
    #[test]
    fn a_thread_followup_waits_for_silence_and_dies_with_the_thread() {
        let policy = OutreachPolicy::default();
        let mut snapshot = eligible();
        snapshot.thread_followup = true;
        snapshot.last_outreach_at = None;

        snapshot.target_last_outreach_at = Some(now() - Duration::days(3));
        assert_eq!(
            evaluate_outreach(snapshot, policy, now()),
            OutreachDecision::Hold(OutreachHoldReason::FollowUpNotDue)
        );

        snapshot.target_last_outreach_at = Some(now() - Duration::days(90));
        assert_eq!(
            evaluate_outreach(snapshot, policy, now()),
            OutreachDecision::Hold(OutreachHoldReason::StaleOpportunity)
        );
    }

    /// One nudge is the whole lane. The send links the thread opportunity to
    /// its own follow-up, and the per-opportunity cap does the rest.
    #[test]
    fn a_thread_followup_sends_exactly_once() {
        let mut snapshot = eligible();
        snapshot.thread_followup = true;
        // The nudge already went out on this opportunity.
        snapshot.last_outreach_at = Some(now() - Duration::days(40));
        snapshot.target_last_outreach_at = Some(now() - Duration::days(40));
        snapshot.followup_count = 1;
        snapshot.lifetime_outbound = 2;
        assert_eq!(
            evaluate_outreach(snapshot, OutreachPolicy::default(), now()),
            OutreachDecision::Hold(OutreachHoldReason::ContactExhausted)
        );
        // A relationship that has ever replied is not silence-capped; the
        // opportunity's own follow-up counter is what keeps it at one nudge.
        snapshot.target_ever_replied = true;
        snapshot.lifetime_outbound = 50;
        assert_eq!(
            evaluate_outreach(snapshot, OutreachPolicy::default(), now()),
            OutreachDecision::Hold(OutreachHoldReason::FollowUpLimit)
        );
        // And a reply at any point ends it, whether it classified or not.
        let mut replied = eligible();
        replied.thread_followup = true;
        replied.target_last_outreach_at = Some(now() - Duration::days(12));
        replied.last_reply = OutreachReplyDisposition::Received;
        assert_eq!(
            evaluate_outreach(replied, OutreachPolicy::default(), now()),
            OutreachDecision::Hold(OutreachHoldReason::AlreadyReplied)
        );
    }
}
