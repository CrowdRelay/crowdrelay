//! Audience lifecycle bounded context.
//!
//! The context decides *whether* a lifecycle touch is appropriate. It never
//! contains an email address and never sends a message; current consent is
//! re-checked again by the delivery boundary immediately before emission.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{FanId, autonomy::Confidence};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct FanLifecycleSnapshot {
    pub fan_id: FanId,
    pub active: bool,
    pub marketing_consent: bool,
    pub created_at: OffsetDateTime,
    pub synesthesia_completed_at: Option<OffsetDateTime>,
    pub last_marketing_touch_at: Option<OffsetDateTime>,
    pub has_paid_ticket: bool,
    pub last_paid_ticket_at: Option<OffsetDateTime>,
    /// Shows this fan has paid for. Needed to tell a first ticket from a fifth,
    /// which is the difference between a true thank-you and an embarrassing one.
    pub paid_ticket_count: u32,
    /// Referrals by this fan that actually converted. Never inferred from
    /// clicks or signups — only a referral the ledger counted as qualified.
    pub qualified_referrals: u32,
    /// When the most recent one converted. Without it the rule cannot say
    /// "recently", and it says nothing rather than guessing.
    pub last_qualified_referral_at: Option<OffsetDateTime>,
    /// Whether this fan already has an active referral code.
    ///
    /// Every message below can carry an invite, and an invite with no code
    /// behind it is a dead end. So the code is issued first, for the same
    /// reason a show gets its tracked link before anything is shared.
    pub has_referral_code: bool,
    pub last_event_interest_at: Option<OffsetDateTime>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct FanLifecyclePolicy {
    /// How recently a milestone must have been crossed to be worth mentioning.
    /// Congratulating somebody on a first ticket they bought in March is worse
    /// than saying nothing.
    pub milestone_recent_hours: u32,
    /// Paid shows at which a fan counts as a returning one.
    pub returning_fan_ticket_threshold: u32,
    pub welcome_after_hours: u32,
    pub minimum_hours_after_synesthesia: u32,
    pub marketing_cooldown_hours: u32,
    pub dormant_after_days: u32,
    /// Days after signup before a fan who has referred nobody is asked to.
    ///
    /// Not zero: the welcome lands first and an invite in the same breath reads
    /// as a transaction rather than a welcome.
    pub referral_invite_after_days: u32,
    /// Days after signup past which the ask stops.
    ///
    /// A bound, not a schedule. Nothing records that a fan has been asked, so
    /// the window is what keeps "ask once or twice" from becoming "ask forever":
    /// with the default cooldown of 120 hours, days 3 to 14 allow at most two.
    /// A fan who has not invited anyone in two weeks has answered.
    pub referral_invite_until_days: u32,
}

impl Default for FanLifecyclePolicy {
    fn default() -> Self {
        Self {
            milestone_recent_hours: 72,
            returning_fan_ticket_threshold: 5,
            welcome_after_hours: 24,
            minimum_hours_after_synesthesia: 48,
            marketing_cooldown_hours: 120,
            dormant_after_days: 60,
            referral_invite_after_days: 3,
            referral_invite_until_days: 14,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleTemplate {
    Welcome,
    SynesthesiaFollowUp,
    DormantReactivation,
    /// Somebody bought their first ticket. The single best moment the band will
    /// ever get to turn a buyer into a fan, and it currently passes in silence.
    FirstTicketThankYou,
    /// Somebody came back often enough that it is worth saying so.
    ReturningFanThankYou,
    /// A referral this fan made actually converted.
    ReferralThankYou,
    /// Ask a fan who has referred nobody to invite someone.
    ///
    /// The one lifecycle step whose purpose is growth rather than
    /// acknowledgement. Every other message *carries* an invite; none of them
    /// asks, and a code nobody is asked to share is a door with no handle --
    /// which is what ten issued codes and one attributed referral looks like.
    ///
    /// Not a milestone: it is an approach, so it waits for the cooldown like
    /// every other approach.
    ReferralInvite,
}

impl LifecycleTemplate {
    /// True when the message is an acknowledgement of something that happened
    /// rather than an approach.
    ///
    /// Milestones are the only lifecycle messages allowed to interrupt the
    /// marketing cooldown, and only because they are time-bound: "thanks for
    /// your first ticket" is worth sending on the day and worthless a month
    /// later, whereas a reactivation can always wait.
    #[must_use]
    pub const fn is_milestone(self) -> bool {
        matches!(
            self,
            Self::FirstTicketThankYou | Self::ReturningFanThankYou | Self::ReferralThankYou
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FanLifecycleDecision {
    Hold(FanLifecycleHoldReason),
    /// Give this fan a referral code before anything invites them to share.
    ///
    /// Costs nothing, reaches nobody outside the workspace, and is the only
    /// growth mechanism that scales with the audience rather than with the
    /// band's effort — which is exactly why it must exist before the campaign
    /// rather than after it.
    IssueReferralCode {
        confidence: Confidence,
    },
    RequestMessage {
        template: LifecycleTemplate,
        confidence: Confidence,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FanLifecycleHoldReason {
    InvalidSnapshot,
    Inactive,
    NoConsent,
    TooEarly,
    CooldownActive,
    AlreadyConverted,
    NoLifecycleOpportunity,
}

/// The one milestone worth acknowledging right now, if any.
///
/// Every branch requires the milestone to have actually been reached *and* to
/// have been reached recently. A count with no timestamp cannot say "recently",
/// so a fan whose ticket date is unknown gets nothing rather than a guess.
///
/// One milestone at a time, strongest first: somebody who hit their fifth show
/// and made a referral in the same week hears about the show, not both.
fn milestone_due(
    snapshot: FanLifecycleSnapshot,
    policy: FanLifecyclePolicy,
    now: OffsetDateTime,
) -> Option<LifecycleTemplate> {
    if policy.milestone_recent_hours == 0 {
        return None;
    }
    let window = Duration::hours(i64::from(policy.milestone_recent_hours));
    let ticket_is_fresh = snapshot
        .last_paid_ticket_at
        .is_some_and(|at| now - at <= window);

    if ticket_is_fresh && snapshot.paid_ticket_count == 1 {
        return Some(LifecycleTemplate::FirstTicketThankYou);
    }
    if ticket_is_fresh
        && policy.returning_fan_ticket_threshold > 0
        && snapshot.paid_ticket_count == policy.returning_fan_ticket_threshold
    {
        // Exactly at the threshold, not past it: otherwise every subsequent
        // ticket re-congratulates the same fan for the same thing.
        return Some(LifecycleTemplate::ReturningFanThankYou);
    }
    if snapshot.qualified_referrals > 0
        && snapshot
            .last_qualified_referral_at
            .is_some_and(|at| now - at <= window)
    {
        // Somebody brought a real person who really came. The cheapest growth
        // there is, and the least often acknowledged.
        return Some(LifecycleTemplate::ReferralThankYou);
    }
    None
}

#[must_use]
pub fn evaluate_fan_lifecycle(
    snapshot: FanLifecycleSnapshot,
    policy: FanLifecyclePolicy,
    now: OffsetDateTime,
) -> FanLifecycleDecision {
    if snapshot.created_at > now
        || snapshot
            .last_qualified_referral_at
            .is_some_and(|at| at > now)
        || snapshot.synesthesia_completed_at.is_some_and(|at| at > now)
        || snapshot.last_marketing_touch_at.is_some_and(|at| at > now)
        || snapshot.last_paid_ticket_at.is_some_and(|at| at > now)
        || snapshot.last_event_interest_at.is_some_and(|at| at > now)
    {
        return FanLifecycleDecision::Hold(FanLifecycleHoldReason::InvalidSnapshot);
    }
    if !snapshot.active {
        return FanLifecycleDecision::Hold(FanLifecycleHoldReason::Inactive);
    }
    if !snapshot.marketing_consent {
        return FanLifecycleDecision::Hold(FanLifecycleHoldReason::NoConsent);
    }
    // A consented fan with no code is a door that does not open. This runs
    // before every message, because each of them can carry an invite and an
    // invite with no code behind it goes nowhere.
    if !snapshot.has_referral_code {
        return FanLifecycleDecision::IssueReferralCode {
            confidence: Confidence::saturating_from_basis_points(9_900),
        };
    }

    // Milestones are checked before the cooldown, and they are the only thing
    // allowed past it. A thank-you for a ticket bought this morning is worth
    // sending this morning; held for five days it becomes strange. Everything
    // else can wait, so everything else waits.
    if let Some(template) = milestone_due(snapshot, policy, now) {
        return FanLifecycleDecision::RequestMessage {
            template,
            confidence: Confidence::saturating_from_basis_points(9_800),
        };
    }

    if snapshot.last_marketing_touch_at.is_some_and(|last_touch| {
        now - last_touch < Duration::hours(i64::from(policy.marketing_cooldown_hours))
    }) {
        return FanLifecycleDecision::Hold(FanLifecycleHoldReason::CooldownActive);
    }

    if snapshot.last_marketing_touch_at.is_none()
        && now - snapshot.created_at >= Duration::hours(i64::from(policy.welcome_after_hours))
    {
        return FanLifecycleDecision::RequestMessage {
            template: LifecycleTemplate::Welcome,
            confidence: Confidence::saturating_from_basis_points(9_700),
        };
    }

    // The ask. Placed after the welcome so it is never a fan's first contact,
    // and before dormancy so it reaches somebody still paying attention.
    //
    // Bounded by a window rather than by a "an asked" flag because nothing records
    // one. Outside the window the fan is left alone: someone who has invited
    // nobody in two weeks has given an answer.
    if snapshot.qualified_referrals == 0
        && snapshot.last_marketing_touch_at.is_some()
        && now - snapshot.created_at >= Duration::days(i64::from(policy.referral_invite_after_days))
        && now - snapshot.created_at < Duration::days(i64::from(policy.referral_invite_until_days))
    {
        return FanLifecycleDecision::RequestMessage {
            template: LifecycleTemplate::ReferralInvite,
            confidence: Confidence::saturating_from_basis_points(8_800),
        };
    }

    if !snapshot.has_paid_ticket
        && let Some(completed_at) = snapshot.synesthesia_completed_at
        && now - completed_at >= Duration::hours(i64::from(policy.minimum_hours_after_synesthesia))
    {
        return FanLifecycleDecision::RequestMessage {
            template: LifecycleTemplate::SynesthesiaFollowUp,
            confidence: Confidence::saturating_from_basis_points(9_000),
        };
    }

    let latest_activity = snapshot
        .last_paid_ticket_at
        .into_iter()
        .chain(snapshot.last_event_interest_at)
        .chain(snapshot.synesthesia_completed_at)
        .max()
        .unwrap_or(snapshot.created_at);
    if now - latest_activity >= Duration::days(i64::from(policy.dormant_after_days)) {
        return FanLifecycleDecision::RequestMessage {
            template: LifecycleTemplate::DormantReactivation,
            confidence: Confidence::saturating_from_basis_points(8_600),
        };
    }

    if snapshot.has_paid_ticket {
        return FanLifecycleDecision::Hold(FanLifecycleHoldReason::AlreadyConverted);
    }
    FanLifecycleDecision::Hold(FanLifecycleHoldReason::NoLifecycleOpportunity)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }
    fn eligible() -> FanLifecycleSnapshot {
        FanLifecycleSnapshot {
            fan_id: FanId::new(),
            active: true,
            marketing_consent: true,
            created_at: now() - Duration::days(10),
            synesthesia_completed_at: None,
            last_marketing_touch_at: None,
            has_paid_ticket: false,
            has_referral_code: true,
            paid_ticket_count: 0,
            qualified_referrals: 0,
            last_qualified_referral_at: None,
            last_paid_ticket_at: None,
            last_event_interest_at: None,
        }
    }
    #[test]
    fn first_touch_is_a_welcome_without_requiring_synesthesia() {
        assert!(matches!(
            evaluate_fan_lifecycle(eligible(), FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::Welcome,
                ..
            }
        ));
    }
    #[test]
    fn consent_is_a_hard_gate() {
        let mut s = eligible();
        s.marketing_consent = false;
        assert_eq!(
            evaluate_fan_lifecycle(s, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::Hold(FanLifecycleHoldReason::NoConsent)
        );
    }
    #[test]
    fn dormant_fans_get_one_reactivation_after_cooldown() {
        let mut s = eligible();
        s.last_marketing_touch_at = Some(now() - Duration::days(90));
        s.created_at = now() - Duration::days(120);
        assert!(matches!(
            evaluate_fan_lifecycle(s, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::DormantReactivation,
                ..
            }
        ));
    }

    #[test]
    fn a_first_ticket_is_thanked_on_the_day_it_happens() {
        // The single best moment the band gets to turn a buyer into a fan, and
        // until now it passed in silence.
        let mut data = eligible();
        data.has_paid_ticket = true;
        data.paid_ticket_count = 1;
        data.last_paid_ticket_at = Some(now() - Duration::hours(2));
        assert_eq!(
            evaluate_fan_lifecycle(data, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::FirstTicketThankYou,
                confidence: Confidence::saturating_from_basis_points(9_800),
            }
        );
    }

    #[test]
    fn a_stale_milestone_is_never_mentioned() {
        // Congratulating somebody on a ticket they bought in March is worse
        // than saying nothing.
        let mut data = eligible();
        data.has_paid_ticket = true;
        data.paid_ticket_count = 1;
        data.last_paid_ticket_at = Some(now() - Duration::days(40));
        assert!(!matches!(
            evaluate_fan_lifecycle(data, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::FirstTicketThankYou,
                ..
            }
        ));
    }

    #[test]
    fn a_returning_fan_is_thanked_once_and_not_at_every_ticket_after() {
        let policy = FanLifecyclePolicy::default();
        let mut data = eligible();
        data.has_paid_ticket = true;
        data.last_paid_ticket_at = Some(now() - Duration::hours(1));

        data.paid_ticket_count = policy.returning_fan_ticket_threshold;
        assert!(matches!(
            evaluate_fan_lifecycle(data, policy, now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::ReturningFanThankYou,
                ..
            }
        ));

        // One past the threshold says nothing: re-congratulating the same fan
        // for the same thing is how a thank-you becomes noise.
        data.paid_ticket_count = policy.returning_fan_ticket_threshold + 1;
        assert!(!matches!(
            evaluate_fan_lifecycle(data, policy, now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::ReturningFanThankYou,
                ..
            }
        ));
    }

    #[test]
    fn a_converted_referral_is_acknowledged() {
        let mut data = eligible();
        data.qualified_referrals = 1;
        data.last_qualified_referral_at = Some(now() - Duration::hours(3));
        assert!(matches!(
            evaluate_fan_lifecycle(data, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::ReferralThankYou,
                ..
            }
        ));
    }

    #[test]
    fn a_referral_count_without_a_date_says_nothing() {
        // The rule cannot claim "recently" without a timestamp, so it does not
        // claim anything.
        let mut data = eligible();
        data.qualified_referrals = 3;
        data.last_qualified_referral_at = None;
        assert!(!matches!(
            evaluate_fan_lifecycle(data, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::ReferralThankYou,
                ..
            }
        ));
    }

    #[test]
    fn a_milestone_passes_the_marketing_cooldown_but_nothing_else_does() {
        let mut data = eligible();
        data.last_marketing_touch_at = Some(now() - Duration::hours(1));

        // Without a milestone the cooldown holds.
        assert_eq!(
            evaluate_fan_lifecycle(data, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::Hold(FanLifecycleHoldReason::CooldownActive)
        );

        // With one it goes, because a same-day thank-you held for five days
        // becomes strange.
        data.has_paid_ticket = true;
        data.paid_ticket_count = 1;
        data.last_paid_ticket_at = Some(now() - Duration::hours(1));
        assert!(matches!(
            evaluate_fan_lifecycle(data, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::FirstTicketThankYou,
                ..
            }
        ));
    }

    #[test]
    fn consent_still_outranks_every_milestone() {
        let mut data = eligible();
        data.marketing_consent = false;
        data.has_paid_ticket = true;
        data.paid_ticket_count = 1;
        data.last_paid_ticket_at = Some(now() - Duration::hours(1));
        assert_eq!(
            evaluate_fan_lifecycle(data, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::Hold(FanLifecycleHoldReason::NoConsent)
        );
    }

    #[test]
    fn only_one_milestone_is_sent_at_a_time() {
        let policy = FanLifecyclePolicy::default();
        let mut data = eligible();
        data.has_paid_ticket = true;
        data.paid_ticket_count = 1;
        data.last_paid_ticket_at = Some(now() - Duration::hours(1));
        data.qualified_referrals = 2;
        data.last_qualified_referral_at = Some(now() - Duration::hours(1));
        assert!(matches!(
            evaluate_fan_lifecycle(data, policy, now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::FirstTicketThankYou,
                ..
            }
        ));
    }

    #[test]
    fn a_fan_without_a_code_gets_one_before_any_message() {
        // Every message can carry an invite, and an invite with no code behind
        // it is a dead end.
        let mut data = eligible();
        data.has_referral_code = false;
        assert_eq!(
            evaluate_fan_lifecycle(data, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::IssueReferralCode {
                confidence: Confidence::saturating_from_basis_points(9_900),
            }
        );
    }

    #[test]
    fn issuing_a_code_outranks_even_a_milestone() {
        // The thank-you should carry a working invite the first time it is
        // sent, not the second.
        let mut data = eligible();
        data.has_referral_code = false;
        data.has_paid_ticket = true;
        data.paid_ticket_count = 1;
        data.last_paid_ticket_at = Some(now() - Duration::hours(1));
        assert!(matches!(
            evaluate_fan_lifecycle(data, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::IssueReferralCode { .. }
        ));
    }

    #[test]
    fn a_fan_without_consent_gets_no_code_either() {
        // A code is harmless, but issuing one for somebody who never agreed to
        // hear from us implies a relationship that does not exist.
        let mut data = eligible();
        data.has_referral_code = false;
        data.marketing_consent = false;
        assert_eq!(
            evaluate_fan_lifecycle(data, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::Hold(FanLifecycleHoldReason::NoConsent)
        );
    }

    #[test]
    fn a_fan_who_already_has_a_code_is_left_alone() {
        let decision = evaluate_fan_lifecycle(eligible(), FanLifecyclePolicy::default(), now());
        assert!(!matches!(
            decision,
            FanLifecycleDecision::IssueReferralCode { .. }
        ));
    }

    /// The one lifecycle step whose purpose is growth. Ten codes were issued and
    /// one referral was ever attributed, because every message could carry an
    /// invite and none of them asked for one.
    #[test]
    fn a_welcomed_fan_who_has_referred_nobody_is_asked_to() {
        let mut snapshot = eligible();
        // Welcomed six days ago: inside the ask window, past the cooldown.
        snapshot.created_at = now() - Duration::days(6);
        snapshot.last_marketing_touch_at = Some(now() - Duration::days(6));
        assert!(matches!(
            evaluate_fan_lifecycle(snapshot, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::ReferralInvite,
                ..
            }
        ));
    }

    #[test]
    fn the_ask_is_never_a_fans_first_contact() {
        // No marketing touch yet: the welcome comes first, always.
        let mut snapshot = eligible();
        snapshot.created_at = now() - Duration::days(6);
        snapshot.last_marketing_touch_at = None;
        assert!(matches!(
            evaluate_fan_lifecycle(snapshot, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::Welcome,
                ..
            }
        ));
    }

    #[test]
    fn a_fan_who_already_referred_someone_is_not_asked_again() {
        let mut snapshot = eligible();
        snapshot.created_at = now() - Duration::days(6);
        snapshot.last_marketing_touch_at = Some(now() - Duration::days(6));
        snapshot.qualified_referrals = 1;
        assert!(!matches!(
            evaluate_fan_lifecycle(snapshot, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::ReferralInvite,
                ..
            }
        ));
    }

    #[test]
    fn the_ask_stops_at_the_end_of_its_window() {
        // Nothing records that a fan has been asked, so the window is the only
        // thing stopping "ask once or twice" becoming "ask forever". Someone who
        // has invited nobody in two weeks has answered.
        let mut snapshot = eligible();
        snapshot.created_at = now() - Duration::days(30);
        snapshot.last_marketing_touch_at = Some(now() - Duration::days(30));
        assert!(!matches!(
            evaluate_fan_lifecycle(snapshot, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::ReferralInvite,
                ..
            }
        ));
    }

    #[test]
    fn the_ask_waits_for_the_cooldown_like_every_other_approach() {
        // Welcomed yesterday: inside the window by age, but the cooldown holds.
        let mut snapshot = eligible();
        snapshot.created_at = now() - Duration::days(6);
        snapshot.last_marketing_touch_at = Some(now() - Duration::hours(24));
        assert!(matches!(
            evaluate_fan_lifecycle(snapshot, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::Hold(FanLifecycleHoldReason::CooldownActive)
        ));
    }
}
