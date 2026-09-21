//! The operator's "yes, for this one, stop asking me".
//!
//! [`crate::action_class`] answers how far an action may reach and
//! [`crate::autonomy`] answers how much authority a context holds. Between
//! them they can express "ask a person about every third-party contact" and
//! "never ask about any of them", and nothing in between. The decision an
//! operator actually makes is in between: after reading three drafts for one
//! community they know that community is fine, and they know nothing new
//! about the next one.
//!
//! Without somewhere to record that, the only way to say it is to approve
//! each post. Approvals expire at 72 hours, and the queue refilled faster
//! than a person emptied it: measured in production, seven drafts, four
//! approvals, four hundred and twelve opportunities and zero posts published.
//!
//! [`crate::action_class`] has promised this since it was written — *"a row
//! update and a set of pre-approved templates"* — and only the row update
//! shipped. This is the other half.
//!
//! # What a grant may do
//!
//! Exactly one thing: satisfy the approval requirement for one named target.
//! [`may_act_unattended`] is the whole rule, and the three refusals in it are
//! the point of the module:
//!
//! - **It cannot act where the operator said not to act.** `Observe` and
//!   `Recommend` produce no action to approve, and a grant does not
//!   manufacture one.
//! - **It cannot cover money.** The migration refuses `paid` with a CHECK, so
//!   no such row exists to be read; [`ActionClass::may_carry_standing_approval`]
//!   says the same thing in Rust so a caller does not have to learn it from a
//!   constraint violation.
//! - **It cannot cover a target nobody named.** There is no wildcard. The key
//!   is one concrete target, so "approve everything" is not expressible.

use time::{Duration, OffsetDateTime};

use crate::action_class::ActionClass;
use crate::autonomy::AutonomyLevel;

/// How long a new grant lasts unless the operator chooses otherwise.
///
/// A grant with no end is a decision nobody revisits: moderators change, the
/// band's standing in a community changes, and the reason the operator said
/// yes decays without announcing itself. Ninety days brings the grant back
/// for a second look while the first look is still remembered — long enough
/// not to be ceremony, short enough that a season of drift cannot hide behind
/// it.
pub const DEFAULT_GRANT_DAYS: i64 = 90;

/// The longest grant an operator may write. A grant outliving the evidence
/// that justified it is the failure this whole module exists to bound, so the
/// ceiling is a rule rather than a convention.
pub const MAX_GRANT_DAYS: i64 = 365;

/// One operator grant, as it is read back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StandingGrant {
    /// What the grant was recorded against. Read back rather than re-derived
    /// so a row written when an action kind carried one class cannot be used
    /// to license that kind after it has been reclassified into another.
    pub class: ActionClass,
    pub expires_at: OffsetDateTime,
    pub revoked_at: Option<OffsetDateTime>,
}

impl StandingGrant {
    /// Whether this grant still speaks for the operator.
    ///
    /// Revocation beats expiry and both beat the grant. A revoked row is kept
    /// rather than deleted — what was trusted, by whom, and until when is the
    /// history an operator needs when the same community comes back.
    #[must_use]
    pub fn is_live(&self, now: OffsetDateTime) -> bool {
        if self.revoked_at.is_some_and(|revoked| revoked <= now) {
            return false;
        }
        // Refuse a class that may never carry a grant even if a row somehow
        // exists for one. The CHECK is the guarantee; this is the belt.
        if !self.class.may_carry_standing_approval() {
            return false;
        }
        self.expires_at > now
    }
}

/// Which standing answer, if any, lets an action run without a person.
///
/// The distinction matters to a caller whose own approval unit is wider than
/// one target: a community relay batch is authorized by `Policy` — the
/// workspace's standing configuration answers for every community the
/// content was drafted into — while `Grant` queues only the one delivery
/// its target covers and leaves the rest of the spread on the card.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnattendedAuthority {
    /// No standing answer; a person must be asked.
    Denied,
    /// The workspace's own standing configuration — the context and class
    /// axes both permitting — answers for every target alike.
    Policy,
    /// A live grant covers this action's one named target, and that target
    /// only.
    Grant,
}

/// The standing answer behind an unattended action, if there is one.
///
/// `authority` is the output of [`crate::action_class::effective_authority`] —
/// the stricter of the context level and the class ceiling. A grant is read
/// only against `RequireApproval`, because that is the only level that is
/// asking a question a standing answer can answer.
#[must_use]
pub fn unattended_authority(
    authority: AutonomyLevel,
    grant: Option<StandingGrant>,
    now: OffsetDateTime,
) -> UnattendedAuthority {
    match authority {
        // The operator already said this may run unattended. A grant adds
        // nothing and its absence takes nothing away.
        AutonomyLevel::BoundedAuto => UnattendedAuthority::Policy,
        // The one question a standing grant answers.
        AutonomyLevel::RequireApproval if grant.is_some_and(|grant| grant.is_live(now)) => {
            UnattendedAuthority::Grant
        }
        // `RequireApproval` without a grant still asks. `Observe` and
        // `Recommend` must not read a grant at all: a grant is an answer to
        // "may this go out", not to "should this exist", and a context an
        // operator dialled back to `recommend` is one they wanted quiet —
        // finding a months-old grant still firing would be the opposite of
        // the control the dial is for.
        _ => UnattendedAuthority::Denied,
    }
}

/// Whether an action may run without a person, given the authority its
/// context and class already agreed on and whatever standing grant covers its
/// target.
#[must_use]
pub fn may_act_unattended(
    authority: AutonomyLevel,
    grant: Option<StandingGrant>,
    now: OffsetDateTime,
) -> bool {
    unattended_authority(authority, grant, now) != UnattendedAuthority::Denied
}

/// When a grant written now should expire, for a caller who did not choose.
///
/// # Errors
/// Returns [`GrantError::TooLong`] when `days` exceeds [`MAX_GRANT_DAYS`], and
/// [`GrantError::NotPositive`] when it is zero or negative — a grant that has
/// already expired is a row that answers nothing and hides the fact that
/// nobody was asked.
pub fn expiry_for(now: OffsetDateTime, days: i64) -> Result<OffsetDateTime, GrantError> {
    if days <= 0 {
        return Err(GrantError::NotPositive);
    }
    if days > MAX_GRANT_DAYS {
        return Err(GrantError::TooLong);
    }
    Ok(now + Duration::days(days))
}

/// Why a requested grant was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum GrantError {
    #[error("a standing approval must last at least one day")]
    NotPositive,
    #[error("a standing approval may not last more than {MAX_GRANT_DAYS} days")]
    TooLong,
    #[error("money may not carry a standing approval")]
    ClassNotGrantable,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_780_000_000).expect("valid timestamp")
    }

    fn live_grant() -> StandingGrant {
        StandingGrant {
            class: ActionClass::ThirdParty,
            expires_at: now() + Duration::days(30),
            revoked_at: None,
        }
    }

    #[test]
    fn a_live_grant_answers_require_approval() {
        assert!(may_act_unattended(
            AutonomyLevel::RequireApproval,
            Some(live_grant()),
            now()
        ));
    }

    #[test]
    fn no_grant_leaves_require_approval_asking() {
        assert!(!may_act_unattended(
            AutonomyLevel::RequireApproval,
            None,
            now()
        ));
    }

    #[test]
    fn an_expired_grant_answers_nothing() {
        let grant = StandingGrant {
            expires_at: now() - Duration::seconds(1),
            ..live_grant()
        };
        assert!(!grant.is_live(now()));
        assert!(!may_act_unattended(
            AutonomyLevel::RequireApproval,
            Some(grant),
            now()
        ));
    }

    #[test]
    fn a_revoked_grant_answers_nothing_even_before_it_expires() {
        let grant = StandingGrant {
            revoked_at: Some(now() - Duration::hours(1)),
            ..live_grant()
        };
        assert!(!grant.is_live(now()));
    }

    /// A revocation scheduled for later has not happened yet. The row is
    /// written with `now()`, so this is a guard against a clock, not a
    /// feature.
    #[test]
    fn a_revocation_in_the_future_has_not_happened() {
        let grant = StandingGrant {
            revoked_at: Some(now() + Duration::hours(1)),
            ..live_grant()
        };
        assert!(grant.is_live(now()));
    }

    /// The refusal that matters most, stated twice: the migration's CHECK
    /// cannot write such a row, and reading one would still not license it.
    #[test]
    fn money_can_never_carry_a_grant() {
        assert!(!ActionClass::Paid.may_carry_standing_approval());
        let grant = StandingGrant {
            class: ActionClass::Paid,
            ..live_grant()
        };
        assert!(!grant.is_live(now()));
        assert!(!may_act_unattended(
            AutonomyLevel::RequireApproval,
            Some(grant),
            now()
        ));
    }

    #[test]
    fn every_other_class_may_carry_a_grant() {
        for class in [
            ActionClass::FirstPartyReversible,
            ActionClass::OwnedAudience,
            ActionClass::ThirdParty,
        ] {
            assert!(class.may_carry_standing_approval(), "{class:?}");
        }
    }

    /// The dial still wins. An operator who pulled a context back to
    /// `recommend` wanted it quiet, and a grant from three months ago must
    /// not be what keeps it talking.
    #[test]
    fn a_grant_cannot_act_where_the_operator_said_not_to() {
        for level in [AutonomyLevel::Observe, AutonomyLevel::Recommend] {
            assert!(!may_act_unattended(level, Some(live_grant()), now()));
        }
    }

    #[test]
    fn bounded_auto_needs_no_grant_and_is_not_narrowed_by_a_dead_one() {
        let dead = StandingGrant {
            expires_at: now() - Duration::days(1),
            ..live_grant()
        };
        assert!(may_act_unattended(AutonomyLevel::BoundedAuto, None, now()));
        assert!(may_act_unattended(
            AutonomyLevel::BoundedAuto,
            Some(dead),
            now()
        ));
    }

    #[test]
    fn expiry_refuses_a_grant_that_is_already_over_or_never_ends() {
        assert_eq!(expiry_for(now(), 0), Err(GrantError::NotPositive));
        assert_eq!(expiry_for(now(), -1), Err(GrantError::NotPositive));
        assert_eq!(
            expiry_for(now(), MAX_GRANT_DAYS + 1),
            Err(GrantError::TooLong)
        );
        assert_eq!(
            expiry_for(now(), DEFAULT_GRANT_DAYS),
            Ok(now() + Duration::days(DEFAULT_GRANT_DAYS))
        );
    }
}
