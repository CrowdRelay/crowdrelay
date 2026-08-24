//! The one dial, and what each setting of it means.
//!
//! This lives beside `AutopilotContext` rather than in the domain for one
//! reason: two of the three surfaces a posture applies are domain vocabulary
//! (class ceilings, envelope switches) and one is not (per-context levels),
//! so the whole template sits where the context list is defined. It holds no
//! I/O of any kind.
//!
//! Twenty-one policy rows, four class ceilings and an envelope are the real
//! authority store; this module is the template that sets them all at once,
//! so "let the agent work" is one decision instead of an afternoon. Two
//! properties are load-bearing:
//!
//! 1. **Applying a posture is a human act.** Nothing here widens on its own,
//!    ever; the operator names the posture and the write records who.
//! 2. **Money never runs unattended.** Every posture keeps `paid` behind
//!    approval, pinned by test — the one thing no amount of trust buys back.

use crowdrelay_domain::action_class::ActionClass;
use crowdrelay_domain::autonomy::AutonomyLevel;
use serde::{Deserialize, Serialize};

use super::model::AutopilotContext;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GrowthPosture {
    /// Sees everything, touches nobody. The agent decides and rehearses: dry
    /// run produces every step it *would* take, ceilings stay shut.
    Grounded,
    /// First-party work runs alone (listings, links, segments, drafts);
    /// outward contact is drafted for one-click approval. This is the
    /// posture the growth loop is worth running at before the list is big.
    Working,
    /// Owned audience sends within budget, cooldown and deliverability;
    /// free third-party pitching runs unattended. Gig applications still
    /// wait for a human — contractual, reputational, irreversible.
    FullSend,
}

impl GrowthPosture {
    pub const ALL: [Self; 3] = [Self::Grounded, Self::Working, Self::FullSend];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Grounded => "grounded",
            Self::Working => "working",
            Self::FullSend => "full_send",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "grounded" => Some(Self::Grounded),
            "working" => Some(Self::Working),
            "full_send" => Some(Self::FullSend),
            _ => None,
        }
    }

    /// The envelope switches this posture implies. Budgets are NOT here: the
    /// operator tuned those numbers and a posture flip must not silently
    /// move them — only the two switches change.
    #[must_use]
    pub const fn envelope(self) -> (bool, bool) {
        match self {
            // Dry run is what makes grounded safe to leave on for a week.
            Self::Grounded => (true, true),
            Self::Working => (true, false),
            Self::FullSend => (true, false),
        }
    }

    /// The class ceiling this posture applies.
    #[must_use]
    pub const fn ceiling(self, class: ActionClass) -> AutonomyLevel {
        match (self, class) {
            // Money never. Pinned by test in every posture: trust does not
            // extend to spend.
            (_, ActionClass::Paid) => AutonomyLevel::RequireApproval,
            // Grounded: nothing executes even if something slips past dry run.
            (Self::Grounded, _) => AutonomyLevel::RequireApproval,
            // Working and full_send share this much: first-party and owned
            // audience run autonomously under the envelope.
            (
                Self::Working | Self::FullSend,
                ActionClass::FirstPartyReversible | ActionClass::OwnedAudience,
            ) => AutonomyLevel::BoundedAuto,
            // The one real widening in full send: free third-party pitching
            // unattended. Working still drafts it.
            (Self::Working, ActionClass::ThirdParty) => AutonomyLevel::RequireApproval,
            (Self::FullSend, ActionClass::ThirdParty) => AutonomyLevel::BoundedAuto,
        }
    }

    /// Why the ceiling moved, recorded beside it so the next reader learns
    /// the posture rather than reverse-engineering it from a date.
    #[must_use]
    pub const fn ceiling_rationale(self) -> &'static str {
        match self {
            Self::Grounded => "posture: grounded — rehearsal only",
            Self::Working => "posture: working — first party autonomous, outward drafted",
            Self::FullSend => "posture: full send — free pitching unattended, money gated",
        }
    }

    /// The autonomy level this posture applies to one context.
    ///
    /// Grounded observes everything. Working lets every detector speak and
    /// drafts every action for approval — the posture an agent earns trust
    /// at, and the one worth staying on until the audience is real.
    /// Full send promotes exactly two things to autonomous: owned-audience
    /// messaging (plays, fan lifecycle) and third-party pitching (outreach,
    /// beacons). Gig work stays human in every posture: one signed contract
    /// outweighs any amount of saved time.
    #[must_use]
    pub const fn context_level(self, context: AutopilotContext) -> AutonomyLevel {
        use AutopilotContext as C;
        match self {
            Self::Grounded => AutonomyLevel::Observe,
            Self::Working => match context {
                // Pure detectors: findings are free, so let them recommend.
                C::GrowthMetrics | C::GrowthDebt | C::OutreachSupply => AutonomyLevel::Recommend,
                _ => AutonomyLevel::RequireApproval,
            },
            Self::FullSend => match context {
                C::GrowthMetrics | C::GrowthDebt | C::OutreachSupply => AutonomyLevel::Recommend,
                // Owned audience, autonomous within envelope + deliverability.
                C::FanLifecycle | C::Plays => AutonomyLevel::BoundedAuto,
                // Free third-party pitching, unattended within budget.
                C::Outreach | C::Beacon => AutonomyLevel::BoundedAuto,
                // Everything else — gigs, money, pricing, experiments — keeps
                // a human in the loop whatever the posture says.
                _ => AutonomyLevel::RequireApproval,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_is_never_autonomous_in_any_posture() {
        for posture in GrowthPosture::ALL {
            assert_eq!(
                posture.ceiling(ActionClass::Paid),
                AutonomyLevel::RequireApproval,
                "{} must keep paid behind approval",
                posture.as_str()
            );
        }
    }

    #[test]
    fn grounding_cannot_execute_anything() {
        for class in [
            ActionClass::FirstPartyReversible,
            ActionClass::OwnedAudience,
            ActionClass::ThirdParty,
            ActionClass::Paid,
        ] {
            assert!(!GrowthPosture::Grounded.ceiling(class).may_auto_execute());
        }
        let (enabled, dry_run) = GrowthPosture::Grounded.envelope();
        assert!(enabled && dry_run, "grounded rehearses");
    }

    #[test]
    fn full_send_widens_third_party_and_nothing_else() {
        assert_eq!(
            GrowthPosture::FullSend.ceiling(ActionClass::ThirdParty),
            AutonomyLevel::BoundedAuto
        );
        assert_eq!(
            GrowthPosture::Working.ceiling(ActionClass::ThirdParty),
            AutonomyLevel::RequireApproval,
            "working still drafts third-party contact"
        );
        assert_eq!(
            GrowthPosture::FullSend.ceiling(ActionClass::Paid),
            AutonomyLevel::RequireApproval
        );
    }

    #[test]
    fn postures_round_trip() {
        for posture in GrowthPosture::ALL {
            assert_eq!(GrowthPosture::parse(posture.as_str()), Some(posture));
        }
        assert_eq!(GrowthPosture::parse("yolo"), None);
    }

    #[test]
    fn gig_and_money_contexts_stay_human_even_at_full_send() {
        use AutopilotContext as C;
        for context in [
            C::LiveOpportunity,
            C::BookingOpportunity,
            C::TicketYield,
            C::MerchPricing,
            C::PromotionBudget,
            C::Funding,
        ] {
            assert_eq!(
                GrowthPosture::FullSend.context_level(context),
                AutonomyLevel::RequireApproval,
                "{} must stay approval-gated at full send",
                context.as_str()
            );
        }
    }

    #[test]
    fn grounded_observes_every_context() {
        for context in AutopilotContext::ALL {
            assert_eq!(
                GrowthPosture::Grounded.context_level(context),
                AutonomyLevel::Observe
            );
        }
    }

    #[test]
    fn full_send_promotes_only_audience_and_free_pitching() {
        use AutopilotContext as C;
        for context in [C::FanLifecycle, C::Plays, C::Outreach, C::Beacon] {
            assert_eq!(
                GrowthPosture::FullSend.context_level(context),
                AutonomyLevel::BoundedAuto
            );
            // ...and working keeps them drafted, which is the whole ladder.
            assert_eq!(
                GrowthPosture::Working.context_level(context),
                AutonomyLevel::RequireApproval
            );
        }
    }
}
