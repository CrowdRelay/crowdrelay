//! Latarnik: a person who carries the band to the next right person.
//!
//! A Latarnik is a *role*, not a kind of human and not a funnel stage. The same
//! person can be a fan, a contact, a referrer and a Latarnik at once; the role
//! lives on the person (`persons`), not on the fan row and not on a beacon.
//! (Beacons — radio, venues, promoters — keep their own semantics; nobody is
//! relabelled to fit.)
//!
//! Until this module the system could only use people who had *already*
//! referred somebody (`ShowGrowthLever::FanAmbassadors`, which requires at least one
//! qualified referral). Nothing created them. The rule here is the opposite: detect advocacy
//! readiness **before** the first referral, from first-party behaviour the
//! system owns, so the ask goes to the fans most likely to say yes and is never
//! made to a fan who has given no reason to expect it.
//!
//! What this module decides: who is a candidate, and whether the right move is a
//! light ask for one referral or an invitation to the role. What it does not
//! decide: wording, sending, or authority. The decision is a function of facts
//! (typed [`FanEvidence`]); a language model never decides who is asked.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

/// Where a person stands in the role. Never a fan-funnel stage: a person may
/// hold this beside any other role, and losing it never changes their fan
/// status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleStatus {
    /// Detected from behaviour; nobody has been asked.
    Candidate,
    /// The ask has been made; waiting on the person.
    Invited,
    Active,
    /// The person (or the band) stepped back; can resume.
    Paused,
    /// Ended. Terminal: a withdrawn or revoked role is never re-created by the
    /// machine — only a person can reopen it.
    Revoked,
}

impl RoleStatus {
    pub const ALL: [Self; 5] = [
        Self::Candidate,
        Self::Invited,
        Self::Active,
        Self::Paused,
        Self::Revoked,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Invited => "invited",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Revoked => "revoked",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == value)
    }

    /// Whether a move is legal. `Revoked` has no exits. Nobody becomes active
    /// without having been invited, and an invitation can lapse back to a
    /// candidate only by being revoked and re-detected by a person's decision.
    #[must_use]
    pub fn may_become(self, next: Self) -> bool {
        if self == next {
            return true;
        }
        match self {
            Self::Revoked => false,
            _ if next == Self::Revoked => true,
            Self::Candidate => next == Self::Invited,
            Self::Invited => matches!(next, Self::Active),
            Self::Active => next == Self::Paused,
            Self::Paused => next == Self::Active,
        }
    }
}

/// What the system has observed about one fan, from rows it owns. Counts and
/// booleans only: no address, no name, no free text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct FanEvidence {
    /// Account open (not closed, suppressed or merged away).
    pub account_open: bool,
    /// Current marketing consent is granted.
    pub consented: bool,
    /// How long they have been a fan, in days.
    pub tenure_days: i64,
    /// A meaningful action in the last 30 days.
    pub active_now: bool,
    /// A meaningful action in the 30 days before that: the fan *stayed*.
    pub active_before: bool,
    /// Distinct kinds of meaningful action in the last 90 days.
    pub distinct_actions_90d: u8,
    /// Observed in the room (check-in or redeemed pass) in the last 90 days.
    pub attended_show_90d: bool,
    /// Bought a ticket or merch, ever.
    pub has_purchased: bool,
    /// Referrals of theirs that qualified, ever.
    pub qualified_referrals: u32,
    /// A suppression, opt-out or do-not-contact exists for them in any role.
    pub suppressed_in_any_role: bool,
    /// The band has already asked this person for a referral or to be a
    /// Latarnik (any status of a role row, or a recorded ask).
    pub already_asked: bool,
}

/// What to do about a fan's advocacy potential.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LatarnikMove {
    /// Invite to the role: the fan has shown the most reason to expect it.
    InviteToLatarnik,
    /// A light, single ask for one referral. Not a role, not a program.
    AskForReferral,
    /// Nothing, with the reason.
    None(NotYet),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NotYet {
    /// Account closed, merged or suppressed in any role.
    NotContactable,
    /// No current marketing consent: never asked for anything.
    NoConsent,
    /// Already asked (or already holds the role): one ask per person.
    AlreadyAsked,
    /// Too new: a fan of days has not yet had a reason to carry anything.
    TooNew,
    /// Not currently active.
    NotActive,
    /// Active, but no evidence they would carry the band to a friend.
    NoAdvocacySignal,
}

/// A fan must have been around this long before anything is asked of them.
pub const MIN_TENURE_DAYS: i64 = 14;

/// Decides the move for one fan. Order is the safety order: contactability and
/// consent first (a person who cannot or must not be asked is never a
/// candidate, however loyal), then the one-ask budget, then readiness.
///
/// *Invite* needs the strongest evidence the system can see before a first
/// referral: stayed active across two windows, was in the room or has bought,
/// and does more than one kind of thing. *Ask for a referral* is the lighter
/// move for a fan who is active and engaged but has not yet shown the depth the
/// role asks for. A fan who has already referred somebody is not a candidate
/// for a first ask — they are what the existing ambassador lever uses.
#[must_use]
pub fn evaluate(evidence: &FanEvidence) -> LatarnikMove {
    if !evidence.account_open || evidence.suppressed_in_any_role {
        return LatarnikMove::None(NotYet::NotContactable);
    }
    if !evidence.consented {
        return LatarnikMove::None(NotYet::NoConsent);
    }
    if evidence.already_asked || evidence.qualified_referrals > 0 {
        return LatarnikMove::None(NotYet::AlreadyAsked);
    }
    if evidence.tenure_days < MIN_TENURE_DAYS {
        return LatarnikMove::None(NotYet::TooNew);
    }
    if !evidence.active_now {
        return LatarnikMove::None(NotYet::NotActive);
    }
    let deep = evidence.active_before
        && (evidence.attended_show_90d || evidence.has_purchased)
        && evidence.distinct_actions_90d >= 2;
    if deep {
        return LatarnikMove::InviteToLatarnik;
    }
    if evidence.distinct_actions_90d >= 2 {
        return LatarnikMove::AskForReferral;
    }
    LatarnikMove::None(NotYet::NoAdvocacySignal)
}

/// How long a role may sit `invited` without an answer before it is released
/// (revoked with a reason) so a silent invitation is not a permanent claim.
pub const INVITE_PATIENCE: Duration = Duration::days(30);

/// Whether an unanswered invitation has outlived its patience.
#[must_use]
pub fn invitation_lapsed(invited_at: OffsetDateTime, now: OffsetDateTime) -> bool {
    now - invited_at > INVITE_PATIENCE
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn deep_fan() -> FanEvidence {
        FanEvidence {
            account_open: true,
            consented: true,
            tenure_days: 60,
            active_now: true,
            active_before: true,
            distinct_actions_90d: 3,
            attended_show_90d: true,
            has_purchased: false,
            qualified_referrals: 0,
            suppressed_in_any_role: false,
            already_asked: false,
        }
    }

    #[test]
    fn a_retained_fan_who_was_in_the_room_is_invited_before_any_referral() {
        assert_eq!(evaluate(&deep_fan()), LatarnikMove::InviteToLatarnik);
        let buyer = FanEvidence {
            attended_show_90d: false,
            has_purchased: true,
            ..deep_fan()
        };
        assert_eq!(evaluate(&buyer), LatarnikMove::InviteToLatarnik);
    }

    #[test]
    fn an_engaged_fan_without_the_depth_gets_the_light_ask_not_the_role() {
        let fresh_streak = FanEvidence {
            active_before: false,
            ..deep_fan()
        };
        assert_eq!(evaluate(&fresh_streak), LatarnikMove::AskForReferral);
        let no_room = FanEvidence {
            attended_show_90d: false,
            has_purchased: false,
            ..deep_fan()
        };
        assert_eq!(evaluate(&no_room), LatarnikMove::AskForReferral);
    }

    #[test]
    fn nobody_who_cannot_or_must_not_be_asked_is_ever_a_candidate() {
        for broken in [
            FanEvidence {
                account_open: false,
                ..deep_fan()
            },
            FanEvidence {
                suppressed_in_any_role: true,
                ..deep_fan()
            },
        ] {
            assert_eq!(
                evaluate(&broken),
                LatarnikMove::None(NotYet::NotContactable)
            );
        }
        let no_consent = FanEvidence {
            consented: false,
            ..deep_fan()
        };
        assert_eq!(evaluate(&no_consent), LatarnikMove::None(NotYet::NoConsent));
    }

    #[test]
    fn one_ask_per_person_and_a_referrer_is_not_a_first_ask() {
        let asked = FanEvidence {
            already_asked: true,
            ..deep_fan()
        };
        assert_eq!(evaluate(&asked), LatarnikMove::None(NotYet::AlreadyAsked));
        let referrer = FanEvidence {
            qualified_referrals: 1,
            ..deep_fan()
        };
        assert_eq!(
            evaluate(&referrer),
            LatarnikMove::None(NotYet::AlreadyAsked)
        );
    }

    #[test]
    fn a_fan_of_days_or_one_gone_quiet_is_left_alone() {
        let new = FanEvidence {
            tenure_days: MIN_TENURE_DAYS - 1,
            ..deep_fan()
        };
        assert_eq!(evaluate(&new), LatarnikMove::None(NotYet::TooNew));
        let quiet = FanEvidence {
            active_now: false,
            ..deep_fan()
        };
        assert_eq!(evaluate(&quiet), LatarnikMove::None(NotYet::NotActive));
        let thin = FanEvidence {
            distinct_actions_90d: 1,
            ..deep_fan()
        };
        assert_eq!(
            evaluate(&thin),
            LatarnikMove::None(NotYet::NoAdvocacySignal)
        );
    }

    #[test]
    fn a_revoked_role_has_no_exits_and_nobody_skips_the_invitation() {
        for next in RoleStatus::ALL {
            assert_eq!(
                RoleStatus::Revoked.may_become(next),
                next == RoleStatus::Revoked,
                "{next:?}"
            );
        }
        assert!(RoleStatus::Candidate.may_become(RoleStatus::Invited));
        assert!(!RoleStatus::Candidate.may_become(RoleStatus::Active));
        assert!(RoleStatus::Invited.may_become(RoleStatus::Active));
        assert!(RoleStatus::Active.may_become(RoleStatus::Paused));
        assert!(RoleStatus::Paused.may_become(RoleStatus::Active));
        assert!(!RoleStatus::Active.may_become(RoleStatus::Candidate));
        for status in RoleStatus::ALL {
            assert_eq!(RoleStatus::parse(status.as_str()), Some(status));
            assert!(status.may_become(RoleStatus::Revoked));
        }
    }

    #[test]
    fn a_silent_invitation_lapses_after_its_patience() {
        let invited = datetime!(2026-10-01 12:00 UTC);
        assert!(!invitation_lapsed(invited, datetime!(2026-10-20 12:00 UTC)));
        assert!(invitation_lapsed(invited, datetime!(2026-11-02 12:00 UTC)));
    }
}
