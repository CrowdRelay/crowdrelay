//! What an action costs and whether it can be taken back.
//!
//! The Autopilot authority ladder answers "how much may this *context* do".
//! That is the wrong axis on its own for an agent that acts unprompted: the
//! `release` context sending a push to consented fans and the `release` context
//! emailing a playlist curator carry completely different risk, and no
//! per-context setting can separate them.
//!
//! So a second ceiling sits above the ladder, keyed by what the action actually
//! does, and the stricter of the two wins. A context at `bounded_auto` emitting
//! a third-party action still requires approval.
//!
//! The ceiling values are operator data, not constants here. Widening the
//! agent's autonomy later is a row update and a set of pre-approved templates,
//! not a rewrite — that is the entire point of this module existing before
//! anything acts.

use serde::{Deserialize, Serialize};

use crate::autonomy::{AutonomyLevel, PolicyDisposition};

/// What an action costs and how far its effects reach.
///
/// Ordered from most to least recoverable, which is also the order in which an
/// operator should be willing to hand over control.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionClass {
    /// Own listings, smart links, referral codes, segments, drafts, internal
    /// scheduling. Costs nothing, reaches nobody outside the workspace, and can
    /// be undone by doing the opposite.
    FirstPartyReversible,
    /// Email, push and in-app messages to fans who opted in. Costs nothing and
    /// reaches people who asked to hear from us — but a sent message cannot be
    /// unsent, which is why it is capped and cooled down rather than unlimited.
    OwnedAudience,
    /// Venues, promoters, curators, press, partners. Reputational and
    /// genuinely irreversible: a bad approach closes a door that stays closed,
    /// and the band only gets one first contact with each of them.
    ThirdParty,
    /// Anything that moves money — ad spend, reorders, shipping, price changes.
    Paid,
}

impl ActionClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FirstPartyReversible => "first_party_reversible",
            Self::OwnedAudience => "owned_audience",
            Self::ThirdParty => "third_party",
            Self::Paid => "paid",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "first_party_reversible" => Some(Self::FirstPartyReversible),
            "owned_audience" => Some(Self::OwnedAudience),
            "third_party" => Some(Self::ThirdParty),
            "paid" => Some(Self::Paid),
            _ => None,
        }
    }

    /// Every class, for seeding and for exhaustive checks.
    #[must_use]
    pub const fn all() -> [Self; 4] {
        [
            Self::FirstPartyReversible,
            Self::OwnedAudience,
            Self::ThirdParty,
            Self::Paid,
        ]
    }

    /// The safest posture, used to seed a workspace and as the fallback when a
    /// ceiling row is missing or unreadable.
    ///
    /// A missing ceiling must never mean "no ceiling". Reading an absent row as
    /// unlimited authority is how an agent ends up mailing a curator because a
    /// migration had not run yet.
    #[must_use]
    pub const fn safest_ceiling(self) -> AutonomyLevel {
        match self {
            Self::FirstPartyReversible | Self::OwnedAudience => AutonomyLevel::BoundedAuto,
            Self::ThirdParty | Self::Paid => AutonomyLevel::RequireApproval,
        }
    }

    /// True when acting reaches someone outside the workspace, and therefore
    /// counts against the outward-touch budget.
    #[must_use]
    pub const fn is_outward(self) -> bool {
        matches!(self, Self::OwnedAudience | Self::ThirdParty)
    }

    /// True when the action must be attributed to a specific consented fan
    /// before it may be taken.
    #[must_use]
    pub const fn requires_fan_consent(self) -> bool {
        matches!(self, Self::OwnedAudience)
    }

    /// Why this class is capped where it is. Shown to an operator changing a
    /// ceiling, so the decision is made with the reason in view.
    #[must_use]
    pub const fn rationale(self) -> &'static str {
        match self {
            Self::FirstPartyReversible => {
                "costs nothing, reaches nobody outside the workspace, and is undone by doing the opposite"
            }
            Self::OwnedAudience => {
                "reaches fans who opted in; free, but a sent message cannot be unsent"
            }
            Self::ThirdParty => {
                "reaches a venue, curator or press contact; the band gets one first approach to each"
            }
            Self::Paid => "moves money and cannot be recovered by changing our minds",
        }
    }
}

/// The authority actually available to one action.
///
/// The stricter of what the context is allowed and what the class permits. Both
/// have to agree before anything runs unattended.
#[must_use]
pub const fn effective_authority(
    context_level: AutonomyLevel,
    class_ceiling: AutonomyLevel,
) -> AutonomyLevel {
    if (context_level as u8) < (class_ceiling as u8) {
        context_level
    } else {
        class_ceiling
    }
}

/// Lowers a disposition the bounded context already reached so it cannot
/// exceed the class ceiling.
///
/// **Only ever downgrades.** A generous ceiling must never promote a decision
/// the context declined to make: the ceiling answers "how far is the agent
/// allowed to go", not "how far should it go". Denial is untouchable — a
/// confidence gate that refused is not a permissions question.
#[must_use]
pub const fn clamp_disposition(
    disposition: PolicyDisposition,
    ceiling: AutonomyLevel,
) -> PolicyDisposition {
    let allowed = match ceiling {
        AutonomyLevel::Observe => PolicyDisposition::ObserveOnly,
        AutonomyLevel::Recommend => PolicyDisposition::RecommendOnly,
        AutonomyLevel::RequireApproval => PolicyDisposition::RequireApproval,
        AutonomyLevel::BoundedAuto => PolicyDisposition::AutoExecute,
    };
    match disposition {
        PolicyDisposition::Deny => PolicyDisposition::Deny,
        current => {
            if reach(current) <= reach(allowed) {
                current
            } else {
                allowed
            }
        }
    }
}

/// How far a disposition actually goes, for comparison only.
const fn reach(disposition: PolicyDisposition) -> u8 {
    match disposition {
        PolicyDisposition::Deny => 0,
        PolicyDisposition::ObserveOnly => 1,
        PolicyDisposition::RecommendOnly => 2,
        PolicyDisposition::RequireApproval => 3,
        PolicyDisposition::AutoExecute => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ceiling_lowers_an_auto_execute_decision_but_never_raises_one() {
        // The case that matters: a context trusted to act alone, emitting an
        // action that reaches a curator.
        assert_eq!(
            clamp_disposition(
                PolicyDisposition::AutoExecute,
                ActionClass::ThirdParty.safest_ceiling()
            ),
            PolicyDisposition::RequireApproval
        );
        // And the reverse must not happen: a permissive ceiling cannot turn a
        // recommendation into an execution.
        assert_eq!(
            clamp_disposition(PolicyDisposition::RecommendOnly, AutonomyLevel::BoundedAuto),
            PolicyDisposition::RecommendOnly
        );
    }

    #[test]
    fn a_denied_decision_is_never_reopened_by_a_ceiling() {
        for ceiling in [
            AutonomyLevel::Observe,
            AutonomyLevel::Recommend,
            AutonomyLevel::RequireApproval,
            AutonomyLevel::BoundedAuto,
        ] {
            assert_eq!(
                clamp_disposition(PolicyDisposition::Deny, ceiling),
                PolicyDisposition::Deny
            );
        }
    }

    #[test]
    fn every_disposition_survives_a_ceiling_that_allows_it() {
        for disposition in [
            PolicyDisposition::ObserveOnly,
            PolicyDisposition::RecommendOnly,
            PolicyDisposition::RequireApproval,
            PolicyDisposition::AutoExecute,
        ] {
            assert_eq!(
                clamp_disposition(disposition, AutonomyLevel::BoundedAuto),
                disposition
            );
        }
    }

    #[test]
    fn an_observe_ceiling_stops_everything_short_of_denial() {
        for disposition in [
            PolicyDisposition::RecommendOnly,
            PolicyDisposition::RequireApproval,
            PolicyDisposition::AutoExecute,
        ] {
            assert_eq!(
                clamp_disposition(disposition, AutonomyLevel::Observe),
                PolicyDisposition::ObserveOnly
            );
        }
    }

    #[test]
    fn the_stricter_of_context_and_class_always_wins() {
        // The case the whole module exists for: a fully trusted context must
        // not be able to mail a curator unattended.
        assert_eq!(
            effective_authority(
                AutonomyLevel::BoundedAuto,
                ActionClass::ThirdParty.safest_ceiling()
            ),
            AutonomyLevel::RequireApproval
        );
        // And a generous ceiling cannot promote a context the operator has
        // deliberately left observing.
        assert_eq!(
            effective_authority(
                AutonomyLevel::Observe,
                ActionClass::OwnedAudience.safest_ceiling()
            ),
            AutonomyLevel::Observe
        );
    }

    #[test]
    fn equal_levels_are_left_alone() {
        for level in [
            AutonomyLevel::Observe,
            AutonomyLevel::Recommend,
            AutonomyLevel::RequireApproval,
            AutonomyLevel::BoundedAuto,
        ] {
            assert_eq!(effective_authority(level, level), level);
        }
    }

    #[test]
    fn the_safest_posture_lets_the_agent_act_only_on_its_own_audience() {
        assert_eq!(
            ActionClass::FirstPartyReversible.safest_ceiling(),
            AutonomyLevel::BoundedAuto
        );
        assert_eq!(
            ActionClass::OwnedAudience.safest_ceiling(),
            AutonomyLevel::BoundedAuto
        );
        assert_eq!(
            ActionClass::ThirdParty.safest_ceiling(),
            AutonomyLevel::RequireApproval
        );
        assert_eq!(
            ActionClass::Paid.safest_ceiling(),
            AutonomyLevel::RequireApproval
        );
    }

    #[test]
    fn no_class_is_ever_seeded_above_approval_without_being_free_and_ours() {
        for class in ActionClass::all() {
            if class.safest_ceiling() == AutonomyLevel::BoundedAuto {
                assert!(
                    matches!(
                        class,
                        ActionClass::FirstPartyReversible | ActionClass::OwnedAudience
                    ),
                    "{} may not start out unattended",
                    class.as_str()
                );
            }
        }
    }

    #[test]
    fn spending_and_third_party_contact_can_never_start_unattended() {
        for class in [ActionClass::ThirdParty, ActionClass::Paid] {
            assert!(!class.safest_ceiling().may_auto_execute());
        }
    }

    #[test]
    fn outward_classes_are_the_ones_that_reach_a_person() {
        assert!(!ActionClass::FirstPartyReversible.is_outward());
        assert!(ActionClass::OwnedAudience.is_outward());
        assert!(ActionClass::ThirdParty.is_outward());
        // Spend is capped by money, not by an outward-touch budget; counting it
        // against the message budget would let an ad buy silence a newsletter.
        assert!(!ActionClass::Paid.is_outward());
    }

    #[test]
    fn only_owned_audience_actions_require_fan_consent() {
        assert!(ActionClass::OwnedAudience.requires_fan_consent());
        for class in [
            ActionClass::FirstPartyReversible,
            ActionClass::ThirdParty,
            ActionClass::Paid,
        ] {
            assert!(!class.requires_fan_consent());
        }
    }

    #[test]
    fn every_class_round_trips_and_explains_itself() {
        for class in ActionClass::all() {
            assert_eq!(ActionClass::parse(class.as_str()), Some(class));
            assert!(!class.rationale().is_empty());
        }
        assert_eq!(ActionClass::parse("unlimited"), None);
    }

    #[test]
    fn the_classes_are_ordered_from_most_to_least_recoverable() {
        let mut classes = ActionClass::all();
        classes.sort_unstable();
        assert_eq!(classes, ActionClass::all());
    }
}
