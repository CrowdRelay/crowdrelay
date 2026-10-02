//! Deterministic next-best-action policy for a publicly observed fan prospect.
//!
//! AI may extract evidence; it does not decide whether to contact a person.
//! This module is the typed boundary between observations and a relationship
//! action. The first slice deliberately supports only the healthiest path:
//! people who already spoke to the tenant in a public context.
//!
//! No decision here grants authority or sends anything.

use serde::{Deserialize, Serialize};

use crate::fan_prospect::ProspectStatus;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FanProspectActionKind {
    Observe,
    EngageInContext,
    InviteToFanbase,
    Hold,
    DoNotContact,
}

impl FanProspectActionKind {
    #[must_use]
    pub const fn priority(self) -> u8 {
        match self {
            Self::InviteToFanbase => 4,
            Self::EngageInContext => 3,
            Self::Observe => 2,
            Self::Hold => 1,
            Self::DoNotContact => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FanProspectMedium {
    SameThreadReply,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FanProspectCtaIntent {
    ContinueConversation,
    RelevantTrackedPath,
    JoinFanbase,
}

/// Facts already established by storage/evidence. No model confidence score is
/// accepted here; the evaluator answers only from these typed facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FanProspectActionInput {
    pub status: ProspectStatus,
    /// At least one observation came from a public conversation the tenant is
    /// already participating in.
    pub has_same_thread_context: bool,
    /// The person explicitly asked how to join/follow/stay connected.
    pub explicit_join_or_follow_intent: bool,
    /// Asked about a show or how/where to hear the music.
    pub question_intent: bool,
    /// Replied, shared, or otherwise interacted under the tenant's content.
    pub warm_engagement: bool,
    /// The tenant has an owned member site that can receive a tracked opt-in.
    pub member_site_ready: bool,
    /// This exact medium has a deterministic tracked join path. The first
    /// executable FAN SCOUT slice supports this only for owned Meta replies.
    pub same_thread_join_capture_supported: bool,
    /// The band said something to this person inside the cooldown (a
    /// `fan_prospect_touches` row). One voice at a time: a person who was just
    /// answered is not answered again because the evidence that earned the
    /// first answer is still on file.
    pub recently_engaged: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct FanProspectDecision {
    pub action: FanProspectActionKind,
    pub medium: Option<FanProspectMedium>,
    pub cta_intent: Option<FanProspectCtaIntent>,
    pub reason: &'static str,
    /// Minimum time before the same class of action should be reconsidered.
    /// None means the state itself, not time, must change first.
    pub cooldown_hours: Option<u32>,
    /// The causal chain this action is expected to make measurable.
    pub measurement: &'static str,
}

/// Chooses the relationship move, not its wording.
///
/// Refusal/suppression and already-owned fans always win. An invitation needs
/// explicit join/follow intent, a safe same-thread context and a configured
/// first-party destination. A mere warm interaction is only engagement.
///
/// This function creates no authority and never contacts anyone.
#[must_use]
pub const fn evaluate_fan_prospect(input: FanProspectActionInput) -> FanProspectDecision {
    use FanProspectActionKind as Action;
    use FanProspectCtaIntent as Cta;
    use FanProspectMedium as Medium;
    use ProspectStatus as Status;

    match input.status {
        Status::Refused | Status::Suppressed => FanProspectDecision {
            action: Action::DoNotContact,
            medium: None,
            cta_intent: None,
            reason: "the person refused or is suppressed; the machine must not contact or reopen them",
            cooldown_hours: None,
            measurement: "none",
        },
        Status::Converted => FanProspectDecision {
            action: Action::Hold,
            medium: None,
            cta_intent: None,
            reason: "the prospect is already a first-party fan; fan lifecycle owns the next action",
            cooldown_hours: None,
            measurement: "fan_lifecycle",
        },
        Status::Held => FanProspectDecision {
            action: Action::Hold,
            medium: None,
            cta_intent: None,
            reason: "the prospect is explicitly held; fresh evidence or a human state change must reopen it",
            cooldown_hours: None,
            measurement: "fresh_evidence_or_human_change -> reevaluate",
        },
        Status::Invited => FanProspectDecision {
            action: Action::Hold,
            medium: None,
            cta_intent: None,
            reason: "an invitation is already outstanding; do not ask again while waiting for a result",
            cooldown_hours: Some(168),
            measurement: "invite -> tracked_join_click -> verified_fan_or_timeout",
        },
        _ if input.explicit_join_or_follow_intent
            && input.has_same_thread_context
            && input.member_site_ready
            && input.same_thread_join_capture_supported =>
        {
            FanProspectDecision {
                action: Action::InviteToFanbase,
                medium: Some(Medium::SameThreadReply),
                cta_intent: Some(Cta::JoinFanbase),
                reason: "the person explicitly asked to stay connected and a tracked first-party join path exists",
                cooldown_hours: Some(168),
                measurement: "same_thread_reply -> tracked_join_click -> verified_fan",
            }
        }
        _ if input.explicit_join_or_follow_intent && !input.member_site_ready => {
            FanProspectDecision {
                action: Action::Hold,
                medium: None,
                cta_intent: None,
                reason: "join intent exists but the tenant has no configured first-party member site",
                cooldown_hours: None,
                measurement: "member_site_ready -> reevaluate",
            }
        }
        _ if input.explicit_join_or_follow_intent && !input.has_same_thread_context => {
            FanProspectDecision {
                action: Action::Hold,
                medium: None,
                cta_intent: None,
                reason: "join intent exists but this slice has no safe contextual medium for the person",
                cooldown_hours: None,
                measurement: "safe_contact_context -> reevaluate",
            }
        }
        _ if input.explicit_join_or_follow_intent
            && !input.same_thread_join_capture_supported =>
        {
            FanProspectDecision {
                action: Action::Hold,
                medium: None,
                cta_intent: None,
                reason: "join intent exists but this platform has no approved same-thread tracked capture path",
                cooldown_hours: None,
                measurement: "safe_join_capture_medium -> reevaluate",
            }
        }
        _ if input.recently_engaged => FanProspectDecision {
            action: Action::Hold,
            medium: None,
            cta_intent: None,
            reason: "the band spoke to this person recently; wait for them to answer before speaking again",
            cooldown_hours: Some(72),
            measurement: "touch -> reply_or_tracked_click -> reevaluate",
        },
        _ if input.has_same_thread_context && input.question_intent => FanProspectDecision {
            action: Action::EngageInContext,
            medium: Some(Medium::SameThreadReply),
            cta_intent: Some(Cta::RelevantTrackedPath),
            reason: "the person asked a concrete question in an existing public conversation",
            cooldown_hours: Some(72),
            measurement: "same_thread_reply -> reply_or_tracked_click -> new_prospect_evidence",
        },
        _ if input.has_same_thread_context && input.warm_engagement => FanProspectDecision {
            action: Action::EngageInContext,
            medium: Some(Medium::SameThreadReply),
            cta_intent: Some(Cta::ContinueConversation),
            reason: "the person is already engaging in a public context where a reply is natural",
            cooldown_hours: Some(72),
            measurement: "same_thread_reply -> reply_or_new_observation -> reevaluate",
        },
        _ => FanProspectDecision {
            action: Action::Observe,
            medium: None,
            cta_intent: None,
            reason: "there is not enough relationship evidence for an active move yet",
            cooldown_hours: Some(168),
            measurement: "new_observation -> reevaluate",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(status: ProspectStatus) -> FanProspectActionInput {
        FanProspectActionInput {
            status,
            has_same_thread_context: true,
            explicit_join_or_follow_intent: false,
            question_intent: false,
            warm_engagement: false,
            member_site_ready: true,
            same_thread_join_capture_supported: true,
            recently_engaged: false,
        }
    }

    #[test]
    fn a_no_always_beats_every_growth_signal() {
        for status in [ProspectStatus::Refused, ProspectStatus::Suppressed] {
            let decision = evaluate_fan_prospect(FanProspectActionInput {
                explicit_join_or_follow_intent: true,
                question_intent: true,
                warm_engagement: true,
                ..input(status)
            });
            assert_eq!(decision.action, FanProspectActionKind::DoNotContact);
            assert_eq!(decision.medium, None);
        }
    }

    #[test]
    fn warmth_is_engagement_not_permission_to_invite() {
        let decision = evaluate_fan_prospect(FanProspectActionInput {
            warm_engagement: true,
            ..input(ProspectStatus::Observed)
        });
        assert_eq!(decision.action, FanProspectActionKind::EngageInContext);
        assert_eq!(
            decision.cta_intent,
            Some(FanProspectCtaIntent::ContinueConversation)
        );
    }

    #[test]
    fn invitation_requires_explicit_intent_context_and_owned_destination() {
        let ready = evaluate_fan_prospect(FanProspectActionInput {
            explicit_join_or_follow_intent: true,
            ..input(ProspectStatus::Warming)
        });
        assert_eq!(ready.action, FanProspectActionKind::InviteToFanbase);

        let no_site = evaluate_fan_prospect(FanProspectActionInput {
            explicit_join_or_follow_intent: true,
            member_site_ready: false,
            ..input(ProspectStatus::Warming)
        });
        assert_eq!(no_site.action, FanProspectActionKind::Hold);

        let no_context = evaluate_fan_prospect(FanProspectActionInput {
            explicit_join_or_follow_intent: true,
            has_same_thread_context: false,
            ..input(ProspectStatus::Warming)
        });
        assert_eq!(no_context.action, FanProspectActionKind::Hold);
    }

    #[test]
    fn a_person_just_answered_is_not_answered_again_unless_they_explicitly_ask_to_join() {
        let asked = FanProspectActionInput {
            question_intent: true,
            ..input(ProspectStatus::Observed)
        };
        assert_eq!(
            evaluate_fan_prospect(asked).action,
            FanProspectActionKind::EngageInContext
        );
        let answered = evaluate_fan_prospect(FanProspectActionInput {
            recently_engaged: true,
            ..asked
        });
        assert_eq!(answered.action, FanProspectActionKind::Hold);
        assert_eq!(answered.cooldown_hours, Some(72));

        let explicit_join = evaluate_fan_prospect(FanProspectActionInput {
            explicit_join_or_follow_intent: true,
            recently_engaged: true,
            ..input(ProspectStatus::Warming)
        });
        assert_eq!(
            explicit_join.action,
            FanProspectActionKind::InviteToFanbase,
            "a person explicitly asking how to join gets the answer they requested"
        );

        let refused = evaluate_fan_prospect(FanProspectActionInput {
            explicit_join_or_follow_intent: true,
            recently_engaged: true,
            ..input(ProspectStatus::Refused)
        });
        assert_eq!(refused.action, FanProspectActionKind::DoNotContact);
    }

    #[test]
    fn explicit_join_waits_when_this_platform_has_no_tracked_capture_path() {
        let decision = evaluate_fan_prospect(FanProspectActionInput {
            explicit_join_or_follow_intent: true,
            same_thread_join_capture_supported: false,
            ..input(ProspectStatus::Observed)
        });
        assert_eq!(decision.action, FanProspectActionKind::Hold);
    }

    #[test]
    fn converted_prospect_leaves_this_engine() {
        let decision = evaluate_fan_prospect(input(ProspectStatus::Converted));
        assert_eq!(decision.action, FanProspectActionKind::Hold);
        assert_eq!(decision.measurement, "fan_lifecycle");
    }
}
