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
}
