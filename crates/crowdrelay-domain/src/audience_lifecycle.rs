//! Audience lifecycle bounded context.
//!
//! The context decides *whether* a lifecycle touch is appropriate. It never
//! contains an email address and never sends a message; current consent is
//! re-checked again by the delivery boundary immediately before emission.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{FanId, autonomy::Confidence};

/// The show this fan most recently scanned into, when there is one.
///
/// The slug and title ride the snapshot so the recall candidate can freeze
/// them into the action it proposes — a second check-in between the
/// decision and the send must not rewrite the night the operator approved.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LifecycleCheckin {
    #[serde(with = "time::serde::rfc3339")]
    pub checked_in_at: OffsetDateTime,
    pub event_slug: String,
    pub event_title: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FanLifecycleSnapshot {
    pub fan_id: FanId,
    pub active: bool,
    pub marketing_consent: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub synesthesia_completed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_marketing_touch_at: Option<OffsetDateTime>,
    pub has_paid_ticket: bool,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_paid_ticket_at: Option<OffsetDateTime>,
    /// Shows this fan has paid for. Needed to tell a first ticket from a fifth,
    /// which is the difference between a true thank-you and an embarrassing one.
    pub paid_ticket_count: u32,
    /// Referrals by this fan that actually converted. Never inferred from
    /// clicks or signups — only a referral the ledger counted as qualified.
    pub qualified_referrals: u32,
    /// When the most recent one converted. Without it the rule cannot say
    /// "recently", and it says nothing rather than guessing.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_qualified_referral_at: Option<OffsetDateTime>,
    /// Whether this fan already has an active referral code.
    ///
    /// Every message below can carry an invite, and an invite with no code
    /// behind it is a dead end. So the code is issued first, for the same
    /// reason a show gets its tracked link before anything is shared.
    pub has_referral_code: bool,
    /// Whether a `signal_installations` row already names this fan — the app
    /// on Android, or a web session that identified itself. This is the
    /// "contactable through Signal" bit the whole funnel is counted on; a fan
    /// without it can only be reached by the email they may never open.
    pub has_signal_install: bool,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_event_interest_at: Option<OffsetDateTime>,
    /// The fan's newest concert check-in, or none. Only the latest matters:
    /// a recall names one night, and the newest one is the night.
    pub recent_checkin: Option<LifecycleCheckin>,
}

impl FanLifecycleSnapshot {
    /// The latest deliberate action, including attendance. Sessions, installs
    /// and messages are reachability/contact receipts, not fan engagement.
    #[must_use]
    pub fn latest_engagement_at(&self) -> Option<OffsetDateTime> {
        self.last_paid_ticket_at
            .into_iter()
            .chain(self.last_event_interest_at)
            .chain(self.synesthesia_completed_at)
            .chain(
                self.recent_checkin
                    .as_ref()
                    .map(|checkin| checkin.checked_in_at),
            )
            .max()
    }
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
    /// Minimum signup age before an engaged fan who has referred nobody is asked to.
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
    /// Minimum signup age before a fan without Signal is asked to open it.
    ///
    /// The welcome lands first: an install ask inside the first message reads
    /// as an app download nag rather than a reason to stay close.
    pub signal_install_ask_after_days: u32,
    /// Days after signup past which the install ask stops.
    ///
    /// Same bound shape as the referral invite: nothing records that a fan was
    /// asked, so the window is what keeps "ask once" honest — days 2 to 9
    /// against the default 120-hour cooldown admit exactly one. A fan who has
    /// not opened Signal in that window has answered.
    pub signal_install_ask_until_days: u32,
    /// Hours after a check-in before the show recall may send.
    ///
    /// Nonzero on purpose: the scan itself sends nothing, and a "great to see
    /// you" arriving while the fan is still at the merch table is the contact
    /// collision the recall exists to avoid — the recall is the first message
    /// a check-in ever triggers, and it lands the next day, not at the door.
    pub show_recall_after_hours: u32,
    /// Hours after a check-in past which the recall stops referencing it.
    ///
    /// A window rather than an "asked" flag: the 120-hour marketing cooldown
    /// already guarantees a recall fires at most once per check-in, so the
    /// bound exists only to keep "you were there" from arriving a week late.
    pub show_recall_until_hours: u32,
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
            signal_install_ask_after_days: 2,
            signal_install_ask_until_days: 9,
            show_recall_after_hours: 18,
            show_recall_until_hours: 60,
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
    /// Ask a confirmed fan to open Signal — the app, or the web session that
    /// B3 made count as the same thing.
    ///
    /// Twenty consented fans and one install is the funnel's quietest leak:
    /// an email-only fan is unreachable by push, and a push-reachable fan is
    /// the difference between a contactable audience and a mailing list. Like
    /// the referral invite it is an approach, not a milestone — it waits for
    /// the cooldown and never precedes the welcome.
    SignalInstallAsk,
    /// A fan checked into a show roughly a day ago. The scan itself sends
    /// nothing; this "you were there" is the first message a check-in ever
    /// triggers, which is also why it outranks the welcome — for a fan the
    /// scan created, the recall *is* the welcome.
    ///
    /// Not a milestone and not a standing approach either: it is a windowed
    /// acknowledgement that still waits out the cooldown, and the cooldown is
    /// what keeps it to once per night even though nothing records that a
    /// recall was sent.
    ShowRecall,
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
    snapshot: &FanLifecycleSnapshot,
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
        || snapshot
            .recent_checkin
            .as_ref()
            .is_some_and(|checkin| checkin.checked_in_at > now)
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
    if let Some(template) = milestone_due(&snapshot, policy, now) {
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

    // The show recall sits between the cooldown and the welcome. Ahead of
    // the welcome because for a fan the scan created, "you were there" is
    // the truer first message; behind the cooldown because a fan already in
    // conversation does not need the night read back to them — the touch
    // they already got stands in for it, and the bound below is what keeps
    // the recall from arriving a week after the encore.
    if let Some(checkin) = snapshot.recent_checkin.as_ref()
        && now - checkin.checked_in_at >= Duration::hours(i64::from(policy.show_recall_after_hours))
        && now - checkin.checked_in_at < Duration::hours(i64::from(policy.show_recall_until_hours))
    {
        return FanLifecycleDecision::RequestMessage {
            template: LifecycleTemplate::ShowRecall,
            confidence: Confidence::saturating_from_basis_points(9_500),
        };
    }

    if snapshot.last_marketing_touch_at.is_none()
        && now - snapshot.created_at >= Duration::hours(i64::from(policy.welcome_after_hours))
    {
        return FanLifecycleDecision::RequestMessage {
            template: LifecycleTemplate::Welcome,
            confidence: Confidence::saturating_from_basis_points(9_700),
        };
    }

    // The install ask. Placed after the welcome so it is never a fan's first
    // contact, and before the referral invite: a fan who opens Signal becomes
    // push-reachable, which is worth more to every later ask than an install
    // deferred behind one. Bounded like the referral invite — a window, not
    // an "asked" flag — so it asks once, then stops.
    if !snapshot.has_signal_install
        && snapshot.last_marketing_touch_at.is_some()
        && now - snapshot.created_at
            >= Duration::days(i64::from(policy.signal_install_ask_after_days))
        && now - snapshot.created_at
            < Duration::days(i64::from(policy.signal_install_ask_until_days))
    {
        return FanLifecycleDecision::RequestMessage {
            template: LifecycleTemplate::SignalInstallAsk,
            confidence: Confidence::saturating_from_basis_points(8_800),
        };
    }

    // The ask. Placed after the welcome so it is never a fan's first contact,
    // and before dormancy so it reaches somebody still paying attention.
    //
    // A welcome receipt and elapsed time are not evidence of a useful
    // experience. Require a real fan action before asking them to share.
    let engaged = snapshot
        .latest_engagement_at()
        .is_some_and(|at| at >= snapshot.created_at && at <= now);
    // Bounded by a window rather than by an "asked" flag: outside the
    // window the fan is left alone, including fans with no observed activity.
    if engaged
        && snapshot.qualified_referrals == 0
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
        .latest_engagement_at()
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
            has_signal_install: true,
            paid_ticket_count: 0,
            qualified_referrals: 0,
            last_qualified_referral_at: None,
            last_paid_ticket_at: None,
            last_event_interest_at: None,
            recent_checkin: None,
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
            evaluate_fan_lifecycle(data.clone(), policy, now()),
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
    fn a_checkin_is_recalled_the_next_day_and_is_the_scanned_fans_first_message() {
        // FAN_100 §B5's governor rule: the scan sends nothing, the recall is
        // the first message. For a fan the door created, "you were there"
        // outranks the welcome.
        let mut data = eligible();
        data.created_at = now() - Duration::hours(26);
        data.recent_checkin = Some(LifecycleCheckin {
            checked_in_at: now() - Duration::hours(26),
            event_slug: "virya-furydate-impala".to_owned(),
            event_title: "Virya + Furydate + Impala".to_owned(),
        });
        assert_eq!(
            evaluate_fan_lifecycle(data, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::ShowRecall,
                confidence: Confidence::saturating_from_basis_points(9_500),
            }
        );
    }

    #[test]
    fn the_recall_waits_out_the_night_and_expires() {
        let mut data = eligible();
        data.created_at = now() - Duration::days(30);
        data.last_marketing_touch_at = Some(now() - Duration::days(7));
        let checkin = LifecycleCheckin {
            checked_in_at: now() - Duration::hours(2),
            event_slug: "virya-furydate-impala".to_owned(),
            event_title: "Virya + Furydate + Impala".to_owned(),
        };
        data.recent_checkin = Some(checkin.clone());
        // Two hours after the scan is still the show: nothing sends.
        assert!(!matches!(
            evaluate_fan_lifecycle(data.clone(), FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::ShowRecall,
                ..
            }
        ));

        // Past the window the night is over; mentioning it reads wrong.
        data.recent_checkin = Some(LifecycleCheckin {
            checked_in_at: now() - Duration::hours(80),
            ..checkin
        });
        assert!(!matches!(
            evaluate_fan_lifecycle(data, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::ShowRecall,
                ..
            }
        ));
    }

    #[test]
    fn the_cooldown_is_the_once_per_night_guard() {
        // The recall is not a milestone: a fan touched yesterday stays quiet,
        // and that same cooldown is what stops a second recall for the same
        // check-in after the first one sent.
        let mut data = eligible();
        data.created_at = now() - Duration::days(30);
        data.last_marketing_touch_at = Some(now() - Duration::hours(40));
        data.recent_checkin = Some(LifecycleCheckin {
            checked_in_at: now() - Duration::hours(26),
            event_slug: "virya-furydate-impala".to_owned(),
            event_title: "Virya + Furydate + Impala".to_owned(),
        });
        assert_eq!(
            evaluate_fan_lifecycle(data, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::Hold(FanLifecycleHoldReason::CooldownActive)
        );
    }

    #[test]
    fn a_checkin_without_consent_sends_nothing() {
        let mut data = eligible();
        data.marketing_consent = false;
        data.recent_checkin = Some(LifecycleCheckin {
            checked_in_at: now() - Duration::hours(26),
            event_slug: "virya-furydate-impala".to_owned(),
            event_title: "Virya + Furydate + Impala".to_owned(),
        });
        assert_eq!(
            evaluate_fan_lifecycle(data, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::Hold(FanLifecycleHoldReason::NoConsent)
        );
    }

    #[test]
    fn a_confirmed_fan_without_signal_is_asked_to_open_it() {
        // Twenty consented fans and one install is the funnel's quietest
        // leak; the ask must actually fire.
        let mut data = eligible();
        data.has_signal_install = false;
        data.last_marketing_touch_at = Some(now() - Duration::days(7));
        data.created_at = now() - Duration::days(4);
        assert_eq!(
            evaluate_fan_lifecycle(data, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::SignalInstallAsk,
                confidence: Confidence::saturating_from_basis_points(8_800),
            }
        );
    }

    #[test]
    fn the_install_ask_is_never_the_first_contact() {
        let mut data = eligible();
        data.has_signal_install = false;
        data.last_marketing_touch_at = None;
        data.created_at = now() - Duration::days(4);
        // No welcome yet — the welcome fires instead, and the ask waits.
        assert!(matches!(
            evaluate_fan_lifecycle(data, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::Welcome,
                ..
            }
        ));
    }

    #[test]
    fn the_install_ask_stops_outside_its_window() {
        // The window is the bound, not an "asked" flag: past it the fan is
        // left alone rather than nagged forever.
        let mut data = eligible();
        data.has_signal_install = false;
        data.last_marketing_touch_at = Some(now() - Duration::days(30));
        data.created_at = now() - Duration::days(30);
        assert!(!matches!(
            evaluate_fan_lifecycle(data, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::SignalInstallAsk,
                ..
            }
        ));
    }

    #[test]
    fn an_installed_fan_is_never_asked_to_install() {
        let mut data = eligible();
        // fixture default already true; make it explicit
        data.has_signal_install = true;
        data.last_marketing_touch_at = Some(now() - Duration::days(7));
        data.created_at = now() - Duration::days(4);
        assert!(!matches!(
            evaluate_fan_lifecycle(data, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::SignalInstallAsk,
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
            evaluate_fan_lifecycle(data.clone(), FanLifecyclePolicy::default(), now()),
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
        snapshot.last_event_interest_at = Some(now() - Duration::days(1));
        assert!(matches!(
            evaluate_fan_lifecycle(snapshot, FanLifecyclePolicy::default(), now()),
            FanLifecycleDecision::RequestMessage {
                template: LifecycleTemplate::ReferralInvite,
                ..
            }
        ));
    }

    #[test]
    fn a_welcome_and_signup_age_alone_never_trigger_referral_invites() {
        let mut snapshot = eligible();
        snapshot.created_at = now() - Duration::days(6);
        snapshot.last_marketing_touch_at = Some(now() - Duration::days(6));
        for activity in [
            None,
            Some(now() + Duration::hours(1)),
            Some(snapshot.created_at - Duration::hours(1)),
        ] {
            snapshot.last_event_interest_at = activity;
            assert!(!matches!(
                evaluate_fan_lifecycle(snapshot.clone(), FanLifecyclePolicy::default(), now()),
                FanLifecycleDecision::RequestMessage {
                    template: LifecycleTemplate::ReferralInvite,
                    ..
                }
            ));
        }
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
