//! What it means for a fan to be *active*, in one place.
//!
//! A signup is not a fan and a follower is not an audience. The number that
//! decides whether any of this worked is how many real people did something
//! meaningful recently — and until now the system had no definition of that at
//! all. `signal/active_fans` counts rows whose account `status` is `'active'`,
//! which is a statement about the account and not about the person: a fan who
//! signed up two years ago and has not opened anything since counts, and a fan
//! who bought a ticket this morning without confirming their address does not.
//!
//! So the definition lives here, once, and every read model derives from it:
//!
//! > **active** = signed up, consented, and at least one meaningful action
//! > inside the window.
//!
//! Deliberately strict about what counts. An email open is not an action, an
//! impression is not an action, and a click the system cannot tie to a person
//! is not an action. Everything in [`MeaningfulAction`] is something a
//! identifiable person chose to do and that left a durable first-party row.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

/// The window a meaningful action has to fall inside. Thirty days is the
/// operator's definition, kept as a constant rather than a magic number so the
/// read models and the metric series cannot drift apart.
pub const ACTIVITY_WINDOW_DAYS: i64 = 30;

/// Something a person chose to do, which left a first-party row.
///
/// Each variant maps to a table this system owns. Nothing here depends on a
/// provider telling us what happened, because a provider's word is not
/// evidence a person did anything.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MeaningfulAction {
    /// Bought a ticket. The strongest signal a fan can send.
    TicketPurchase,
    /// Bought merch.
    MerchPurchase,
    /// Referred somebody who actually converted.
    QualifiedReferral,
    /// Said they are coming to a show.
    EventInterest,
    /// Finished a Synesthesia run — a real session, not a synthetic one.
    SynesthesiaRun,
    /// Used the Signal app.
    SignalSession,
}

impl MeaningfulAction {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TicketPurchase => "ticket_purchase",
            Self::MerchPurchase => "merch_purchase",
            Self::QualifiedReferral => "qualified_referral",
            Self::EventInterest => "event_interest",
            Self::SynesthesiaRun => "synesthesia_run",
            Self::SignalSession => "signal_session",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "ticket_purchase" => Some(Self::TicketPurchase),
            "merch_purchase" => Some(Self::MerchPurchase),
            "qualified_referral" => Some(Self::QualifiedReferral),
            "event_interest" => Some(Self::EventInterest),
            "synesthesia_run" => Some(Self::SynesthesiaRun),
            "signal_session" => Some(Self::SignalSession),
            _ => None,
        }
    }

    #[must_use]
    pub const fn all() -> [Self; 6] {
        [
            Self::TicketPurchase,
            Self::MerchPurchase,
            Self::QualifiedReferral,
            Self::EventInterest,
            Self::SynesthesiaRun,
            Self::SignalSession,
        ]
    }

    /// True when the action banks value rather than only showing interest.
    ///
    /// Used to report activation honestly rather than to gate it: a campaign
    /// that produced a thousand event interests and no purchases is a different
    /// result from one that produced a hundred ticket buyers, and a single
    /// "activated" count hides the difference.
    #[must_use]
    pub const fn is_downstream(self) -> bool {
        matches!(
            self,
            Self::TicketPurchase | Self::MerchPurchase | Self::QualifiedReferral
        )
    }
}

/// One fan's activity, as the adapter can observe it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct FanActivity {
    pub consented: bool,
    /// Account is not closed or suppressed.
    pub account_open: bool,
    /// The most recent meaningful action, and what it was. `None` when the fan
    /// has never done any of them.
    pub last_action: Option<(MeaningfulAction, OffsetDateTime)>,
}

/// Why a fan is not counted as active. Carried rather than collapsed into a
/// boolean so a campaign readout can say *which* wall people are hitting —
/// "they never consented" and "they consented and then did nothing" call for
/// completely different responses.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InactiveReason {
    AccountClosed,
    NoConsent,
    NeverActed,
    WindowExpired,
}

impl InactiveReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AccountClosed => "account_closed",
            Self::NoConsent => "no_consent",
            Self::NeverActed => "never_acted",
            Self::WindowExpired => "window_expired",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum ActivationState {
    Active {
        action: MeaningfulAction,
        /// Hours since it happened, so a readout can distinguish somebody who
        /// acted this morning from somebody who acted 29 days ago.
        hours_since: u32,
    },
    Inactive(InactiveReason),
}

impl ActivationState {
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Active { .. })
    }
}

/// Decides whether one fan counts as active right now.
///
/// The order is the order a campaign readout wants to read: a closed account is
/// not a consent problem, and a consented fan who never acted is a different
/// failure from one whose action fell out of the window.
#[must_use]
pub fn activation_state(activity: &FanActivity, now: OffsetDateTime) -> ActivationState {
    if !activity.account_open {
        return ActivationState::Inactive(InactiveReason::AccountClosed);
    }
    if !activity.consented {
        return ActivationState::Inactive(InactiveReason::NoConsent);
    }
    let Some((action, occurred_at)) = activity.last_action else {
        return ActivationState::Inactive(InactiveReason::NeverActed);
    };
    // An action stamped in the future is a clock problem, not activity. Treating
    // it as active would let a bad import inflate the only number that matters.
    if occurred_at > now {
        return ActivationState::Inactive(InactiveReason::WindowExpired);
    }
    let elapsed = now - occurred_at;
    if elapsed > Duration::days(ACTIVITY_WINDOW_DAYS) {
        return ActivationState::Inactive(InactiveReason::WindowExpired);
    }
    ActivationState::Active {
        action,
        hours_since: u32::try_from(elapsed.whole_hours().max(0)).unwrap_or(u32::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acted(action: MeaningfulAction, days_ago: i64) -> FanActivity {
        FanActivity {
            consented: true,
            account_open: true,
            last_action: Some((action, now() - Duration::days(days_ago))),
        }
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    #[test]
    fn a_consented_fan_who_did_something_recently_is_active() {
        let state = activation_state(&acted(MeaningfulAction::TicketPurchase, 3), now());
        assert!(state.is_active());
        assert_eq!(
            state,
            ActivationState::Active {
                action: MeaningfulAction::TicketPurchase,
                hours_since: 72
            }
        );
    }

    #[test]
    fn signing_up_is_not_being_active() {
        // The whole point of the definition: a signup is not a fan.
        let state = activation_state(
            &FanActivity {
                consented: true,
                account_open: true,
                last_action: None,
            },
            now(),
        );
        assert_eq!(state, ActivationState::Inactive(InactiveReason::NeverActed));
    }

    #[test]
    fn the_window_is_exactly_thirty_days() {
        assert!(activation_state(&acted(MeaningfulAction::EventInterest, 30), now()).is_active());
        assert_eq!(
            activation_state(&acted(MeaningfulAction::EventInterest, 31), now()),
            ActivationState::Inactive(InactiveReason::WindowExpired)
        );
    }

    #[test]
    fn consent_and_an_open_account_are_both_required() {
        let mut activity = acted(MeaningfulAction::MerchPurchase, 1);
        activity.consented = false;
        assert_eq!(
            activation_state(&activity, now()),
            ActivationState::Inactive(InactiveReason::NoConsent)
        );

        let mut activity = acted(MeaningfulAction::MerchPurchase, 1);
        activity.account_open = false;
        assert_eq!(
            activation_state(&activity, now()),
            ActivationState::Inactive(InactiveReason::AccountClosed)
        );
    }

    #[test]
    fn a_closed_account_is_not_reported_as_a_consent_problem() {
        // The reasons exist so a campaign readout can tell which wall people
        // are hitting; collapsing them would hide the actionable one.
        let activity = FanActivity {
            consented: false,
            account_open: false,
            last_action: None,
        };
        assert_eq!(
            activation_state(&activity, now()),
            ActivationState::Inactive(InactiveReason::AccountClosed)
        );
    }

    #[test]
    fn an_action_stamped_in_the_future_never_counts() {
        // A bad import must not be able to inflate the only number that matters.
        let activity = FanActivity {
            consented: true,
            account_open: true,
            last_action: Some((MeaningfulAction::SignalSession, now() + Duration::days(1))),
        };
        assert_eq!(
            activation_state(&activity, now()),
            ActivationState::Inactive(InactiveReason::WindowExpired)
        );
    }

    #[test]
    fn purchases_and_referrals_are_downstream_and_interest_is_not() {
        // A thousand event interests and no purchases is a different result
        // from a hundred buyers, and one count would hide it.
        for action in [
            MeaningfulAction::TicketPurchase,
            MeaningfulAction::MerchPurchase,
            MeaningfulAction::QualifiedReferral,
        ] {
            assert!(action.is_downstream());
        }
        for action in [
            MeaningfulAction::EventInterest,
            MeaningfulAction::SynesthesiaRun,
            MeaningfulAction::SignalSession,
        ] {
            assert!(!action.is_downstream());
        }
    }

    #[test]
    fn every_action_and_reason_round_trips() {
        for action in MeaningfulAction::all() {
            assert_eq!(MeaningfulAction::parse(action.as_str()), Some(action));
        }
        assert_eq!(MeaningfulAction::parse("email_open"), None);
        for reason in [
            InactiveReason::AccountClosed,
            InactiveReason::NoConsent,
            InactiveReason::NeverActed,
            InactiveReason::WindowExpired,
        ] {
            assert!(!reason.as_str().is_empty());
        }
    }
}
