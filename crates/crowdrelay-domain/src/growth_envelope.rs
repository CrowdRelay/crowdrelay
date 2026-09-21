//! The limits inside which the growth agent may act without being asked.
//!
//! The class ceiling in [`crate::action_class`] answers *what kind* of thing
//! the agent may do alone. This module answers *how much* — and the second
//! question is the one that decides whether an operator sleeps.
//!
//! A rule with no cap is a rule that behaves perfectly until the day a segment
//! query is wrong, and then mails everyone. Every limit here exists because its
//! absence has a specific failure: no weekly budget means an unbounded send, no
//! cooldown means one fan hears from four plays in a morning, no blast radius
//! means a bad segment costs the whole list instead of thirty addresses, and no
//! kill switch means the only way to stop the agent is a deploy.
//!
//! Nothing here is stored twice. Outward touches are already durable rows in
//! `viryaos_autopilot_actions`; the envelope counts them rather than keeping a
//! second ledger that could disagree.

use serde::{Deserialize, Serialize};

use crate::action_class::ActionClass;

/// Operator-set limits for one workspace.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct GrowthEnvelope {
    /// The kill switch. Off by default: an agent that starts acting the moment
    /// its migration lands is one nobody chose to switch on.
    pub agent_enabled: bool,
    /// Produce every decision with its full evidence and execute nothing. How a
    /// new play earns trust before it is allowed to send anything.
    pub dry_run: bool,
    /// Messages to our own consented fans in a rolling seven days.
    pub weekly_owned_audience_touches: u32,
    /// Approaches to venues, curators and press in a rolling seven days. Low on
    /// purpose even once the ceiling is widened: these are finite relationships.
    pub weekly_third_party_touches: u32,
    /// Third-party approaches in a rolling twenty-four hours — the ceiling that
    /// sits atop the per-subject cooldown. The weekly budget bounds a week;
    /// this bounds a bad morning: a wrong segment that mails every venue at
    /// 09:00 cannot be unmailed at 09:05.
    pub daily_third_party_touches: u32,
    /// No subject hears from the agent twice inside this many hours, whichever
    /// play wants to reach them.
    pub subject_cooldown_hours: u32,
    /// Most recipients one step may reach. Bounds the cost of a wrong segment.
    pub max_recipients_per_step: u32,
    /// Decisions the agent may put in front of a person in a rolling seven
    /// days.
    ///
    /// # Why asking needs a budget
    ///
    /// Every other quantity here bounds what somebody *outside* the workspace
    /// receives. Nothing bounded what the tenant's own crew received, and that
    /// is the quantity that ran out: 26 contexts at 50 actions a day each,
    /// every `awaiting_approval` action becoming an assignment, and every
    /// assignment owing a first notice plus up to three reminders.
    ///
    /// The effect is worse than volume. The agent's authority ladder makes
    /// "ask a person" the cheapest move available to it in almost every
    /// context, and nothing charged it for that move — so an agent behaving
    /// correctly produced a queue nobody could empty, and the approvals
    /// expired at 72 hours faster than they were read. Measured in
    /// production: seven drafts, four approvals, four hundred and twelve
    /// opportunities, zero posts published.
    ///
    /// With a budget, asking stops being free. When it is spent the finding
    /// still exists and still surfaces — it becomes a recommendation instead
    /// of an action, so nothing is lost except the interruption.
    ///
    /// Zero means never ask: every finding stays a recommendation. That is a
    /// coherent posture for a tenant who reads the board and wants no mail,
    /// and it is deliberately expressible.
    pub weekly_approval_requests: u32,
    /// Unattended actions one context may take in a rolling seven days while
    /// it is still below its evidence floor.
    ///
    /// The floor downgrades unattended execution to approval until a context
    /// has twenty resolved outcomes, and acting is the only way those get
    /// made — so with no warm-up the floor is never reached. Measured: zero
    /// resolved outcomes against a floor of twenty. Zero here restores that
    /// behaviour and reads in code as a deliberate "no warm-up", not as an
    /// absent setting. See [`crate::autonomy::BootstrapAllowance`].
    pub weekly_bootstrap_actions: u32,
    /// Tenant is parked: the autopilot cycle returns immediately without
    /// evaluating or producing anything. Set by the Control Plane on park,
    /// cleared on resume. Distinct from `agent_enabled` (which only holds
    /// outward contact) — parked stops the entire cycle.
    pub parked: bool,
}

impl Default for GrowthEnvelope {
    fn default() -> Self {
        Self {
            agent_enabled: false,
            dry_run: true,
            // Deliberately timid. These are the numbers a nervous operator
            // would pick, and raising them is a row update.
            weekly_owned_audience_touches: 200,
            weekly_third_party_touches: 10,
            // A handful of real approaches a day is a band hustling; more is
            // a blast. Per-subject cooldown stays the sharper instrument —
            // this is the volume wall behind it.
            daily_third_party_touches: 3,
            subject_cooldown_hours: 168,
            max_recipients_per_step: 250,
            // Twenty decisions a week is about three a day, which is a number
            // a band can actually work through. It is deliberately of the
            // same order as the crew's per-member ask ceiling: the two bound
            // the same scarce thing from two directions, and a workspace-wide
            // budget far above the per-person one would be no budget at all.
            weekly_approval_requests: 20,
            // Small enough that a mistake is five actions rather than a
            // campaign, large enough that twenty observations are a month
            // away rather than never.
            weekly_bootstrap_actions: 5,
            parked: false,
        }
    }
}

impl GrowthEnvelope {
    /// The weekly budget for one class, where it has one.
    ///
    /// First-party work is not budgeted: updating our own listing costs nobody
    /// anything, and a cap on it would only stop the agent tidying up. Spend is
    /// not budgeted here either — money needs a ledger with a hard stop, and
    /// counting it against a message budget would let an ad buy silence a
    /// newsletter.
    #[must_use]
    pub const fn weekly_budget(&self, class: ActionClass) -> Option<u32> {
        match class {
            ActionClass::OwnedAudience => Some(self.weekly_owned_audience_touches),
            ActionClass::ThirdParty => Some(self.weekly_third_party_touches),
            ActionClass::FirstPartyReversible | ActionClass::Paid => None,
        }
    }

    /// The daily budget for one class, where it has one. Only third-party
    /// work gets a second wall: an owned-audience burst reaches people who
    /// asked to hear from the band, a third-party burst spends a finite
    /// reputation the band cannot buy back.
    #[must_use]
    pub const fn daily_budget(&self, class: ActionClass) -> Option<u32> {
        match class {
            ActionClass::ThirdParty => Some(self.daily_third_party_touches),
            ActionClass::OwnedAudience | ActionClass::FirstPartyReversible | ActionClass::Paid => {
                None
            }
        }
    }
}

/// What the workspace has already spent, measured from the durable action rows.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct EnvelopeUsage {
    pub owned_audience_touches_7d: u32,
    pub third_party_touches_7d: u32,
    /// Third-party approaches in the last 24 hours — the spend the daily
    /// ceiling measures against.
    pub third_party_touches_24h: u32,
    /// Hours since the agent last reached *this* subject through any outward
    /// action. `None` when it never has.
    pub hours_since_subject_touched: Option<u32>,
    /// Decisions the agent put in front of a person in the last seven days.
    ///
    /// Counted from the durable action rows, like every other spend here: an
    /// action that was ever parked for approval carries an
    /// `approval_expires_at`, and that mark survives the approval, so the
    /// count is of asks made rather than of asks still waiting. An operator
    /// who answers quickly has still been asked.
    pub approval_requests_7d: u32,
}

impl EnvelopeUsage {
    #[must_use]
    pub const fn spent(&self, class: ActionClass) -> u32 {
        match class {
            ActionClass::OwnedAudience => self.owned_audience_touches_7d,
            ActionClass::ThirdParty => self.third_party_touches_7d,
            ActionClass::FirstPartyReversible | ActionClass::Paid => 0,
        }
    }

    /// The last 24 hours' spend for classes with a daily wall. Classes
    /// without one read as zero — there is nothing for it to measure.
    #[must_use]
    pub const fn spent_today(&self, class: ActionClass) -> u32 {
        match class {
            ActionClass::ThirdParty => self.third_party_touches_24h,
            ActionClass::OwnedAudience | ActionClass::FirstPartyReversible | ActionClass::Paid => 0,
        }
    }
}

/// Why the envelope is holding an action back.
///
/// Carried rather than collapsed into a boolean so the operator brief can say
/// which limit stopped the work — "budget exhausted" and "agent switched off"
/// call for completely different responses.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum EnvelopeBlock {
    /// The kill switch is off.
    AgentDisabled,
    /// Dry run: the decision is real, the send is not.
    DryRun,
    WeeklyBudgetExhausted {
        spent: u32,
        budget: u32,
    },
    /// The rolling day ran out before the rolling week did.
    DailyBudgetExhausted {
        spent: u32,
        budget: u32,
    },
    SubjectInCooldown {
        hours_remaining: u32,
    },
}

impl EnvelopeBlock {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentDisabled => "agent_disabled",
            Self::DryRun => "dry_run",
            Self::WeeklyBudgetExhausted { .. } => "weekly_budget_exhausted",
            Self::DailyBudgetExhausted { .. } => "daily_budget_exhausted",
            Self::SubjectInCooldown { .. } => "subject_in_cooldown",
        }
    }

    /// Whether the finding should still be offered to a human.
    ///
    /// Dry run is the one block that must not produce an approvable action: the
    /// whole point is that the operator is inspecting what *would* happen, and
    /// an approve button in that view turns a rehearsal into a send.
    #[must_use]
    pub const fn may_offer_for_approval(self) -> bool {
        !matches!(self, Self::DryRun)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum EnvelopeVerdict {
    Allow,
    Hold(EnvelopeBlock),
}

/// Decides whether one outward action fits inside the envelope.
///
/// Order matters and is not arbitrary. The kill switch is checked before
/// anything else because "the operator turned it off" outranks every other
/// consideration. Dry run comes next so a rehearsal never consults a budget it
/// is not going to spend. Only then do the counted limits apply.
#[must_use]
pub fn check_envelope(
    class: ActionClass,
    envelope: &GrowthEnvelope,
    usage: &EnvelopeUsage,
) -> EnvelopeVerdict {
    // First-party work is not governed by the envelope: it reaches nobody, and
    // stopping the agent from fixing its own listings would make the kill
    // switch a rollback of housekeeping rather than a stop on contact.
    if !class.is_outward() {
        return EnvelopeVerdict::Allow;
    }
    if !envelope.agent_enabled {
        return EnvelopeVerdict::Hold(EnvelopeBlock::AgentDisabled);
    }
    if envelope.dry_run {
        return EnvelopeVerdict::Hold(EnvelopeBlock::DryRun);
    }
    if let Some(budget) = envelope.weekly_budget(class) {
        let spent = usage.spent(class);
        if spent >= budget {
            return EnvelopeVerdict::Hold(EnvelopeBlock::WeeklyBudgetExhausted { spent, budget });
        }
    }
    if let Some(budget) = envelope.daily_budget(class) {
        let spent = usage.spent_today(class);
        if spent >= budget {
            return EnvelopeVerdict::Hold(EnvelopeBlock::DailyBudgetExhausted { spent, budget });
        }
    }
    if let Some(hours) = usage.hours_since_subject_touched
        && hours < envelope.subject_cooldown_hours
    {
        return EnvelopeVerdict::Hold(EnvelopeBlock::SubjectInCooldown {
            hours_remaining: envelope.subject_cooldown_hours.saturating_sub(hours),
        });
    }
    EnvelopeVerdict::Allow
}

/// Whether the agent may put another decision in front of a person this week.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "verdict")]
pub enum AttentionVerdict {
    /// There is room in the budget. Park the action and ask.
    Ask,
    /// The budget is spent. The finding still surfaces, as a recommendation.
    Exhausted { spent: u32, budget: u32 },
}

impl AttentionVerdict {
    #[must_use]
    pub const fn may_ask(self) -> bool {
        matches!(self, Self::Ask)
    }
}

/// Decides whether one more approval request fits inside the week.
///
/// Separate from [`check_envelope`] rather than folded into it, for two
/// reasons that both matter:
///
/// 1. `check_envelope` returns early for classes that are not outward,
///    because updating our own listing reaches nobody. An approval request
///    reaches somebody whatever the class is — a person is asked about a
///    first-party action exactly as much as about a third-party one — so this
///    limit cannot live behind that early return.
/// 2. The two bound different populations. The envelope bounds what the
///    audience receives; this bounds what the tenant receives. Collapsing
///    them would let a quiet week with the audience buy a loud one with the
///    band, which is the trade nobody wants.
///
/// The kill switch and dry run are deliberately not consulted here. Both are
/// already decided before an action exists, and a rehearsal asks nobody.
#[must_use]
pub const fn check_attention(envelope: &GrowthEnvelope, usage: &EnvelopeUsage) -> AttentionVerdict {
    if usage.approval_requests_7d < envelope.weekly_approval_requests {
        return AttentionVerdict::Ask;
    }
    AttentionVerdict::Exhausted {
        spent: usage.approval_requests_7d,
        budget: envelope.weekly_approval_requests,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running() -> GrowthEnvelope {
        GrowthEnvelope {
            agent_enabled: true,
            dry_run: false,
            ..GrowthEnvelope::default()
        }
    }

    fn held(verdict: EnvelopeVerdict) -> EnvelopeBlock {
        match verdict {
            EnvelopeVerdict::Hold(block) => block,
            EnvelopeVerdict::Allow => panic!("expected the envelope to hold this action"),
        }
    }

    #[test]
    fn a_new_workspace_has_an_agent_that_is_off_and_rehearsing() {
        // Both, not either: switching the agent on should not also be the
        // moment it first sends something real.
        let envelope = GrowthEnvelope::default();
        assert!(!envelope.agent_enabled);
        assert!(envelope.dry_run);
    }

    #[test]
    fn the_kill_switch_stops_outward_contact_and_nothing_else() {
        let envelope = GrowthEnvelope {
            agent_enabled: false,
            ..running()
        };
        assert_eq!(
            held(check_envelope(
                ActionClass::OwnedAudience,
                &envelope,
                &EnvelopeUsage::default()
            )),
            EnvelopeBlock::AgentDisabled
        );
        // Housekeeping continues. A kill switch that also stopped the agent
        // fixing its own listings would be a rollback, not a stop.
        assert_eq!(
            check_envelope(
                ActionClass::FirstPartyReversible,
                &envelope,
                &EnvelopeUsage::default()
            ),
            EnvelopeVerdict::Allow
        );
    }

    #[test]
    fn dry_run_holds_everything_outward_without_consulting_a_budget() {
        let envelope = GrowthEnvelope {
            dry_run: true,
            ..running()
        };
        let usage = EnvelopeUsage {
            owned_audience_touches_7d: 0,
            ..EnvelopeUsage::default()
        };
        assert_eq!(
            held(check_envelope(
                ActionClass::OwnedAudience,
                &envelope,
                &usage
            )),
            EnvelopeBlock::DryRun
        );
    }

    #[test]
    fn a_rehearsal_never_produces_something_approvable() {
        // An approve button in a dry-run view turns a rehearsal into a send.
        assert!(!EnvelopeBlock::DryRun.may_offer_for_approval());
        for block in [
            EnvelopeBlock::AgentDisabled,
            EnvelopeBlock::WeeklyBudgetExhausted {
                spent: 5,
                budget: 5,
            },
            EnvelopeBlock::SubjectInCooldown { hours_remaining: 3 },
        ] {
            assert!(block.may_offer_for_approval());
        }
    }

    #[test]
    fn the_weekly_budget_stops_at_the_budget_not_after_it() {
        let envelope = GrowthEnvelope {
            weekly_owned_audience_touches: 5,
            ..running()
        };
        let usage = |spent| EnvelopeUsage {
            owned_audience_touches_7d: spent,
            ..EnvelopeUsage::default()
        };
        assert_eq!(
            check_envelope(ActionClass::OwnedAudience, &envelope, &usage(4)),
            EnvelopeVerdict::Allow
        );
        // The fifth send is the budget, so the sixth is refused — an
        // off-by-one here is a send nobody authorized.
        assert_eq!(
            held(check_envelope(
                ActionClass::OwnedAudience,
                &envelope,
                &usage(5)
            )),
            EnvelopeBlock::WeeklyBudgetExhausted {
                spent: 5,
                budget: 5
            }
        );
    }

    #[test]
    fn each_outward_class_spends_its_own_budget() {
        // A busy newsletter week must not silence curator outreach, and a wave
        // of pitches must not eat the audience budget.
        let envelope = GrowthEnvelope {
            weekly_owned_audience_touches: 1,
            weekly_third_party_touches: 1,
            ..running()
        };
        let usage = EnvelopeUsage {
            owned_audience_touches_7d: 50,
            third_party_touches_7d: 0,
            third_party_touches_24h: 0,
            hours_since_subject_touched: None,
            approval_requests_7d: 0,
        };
        assert!(matches!(
            check_envelope(ActionClass::OwnedAudience, &envelope, &usage),
            EnvelopeVerdict::Hold(_)
        ));
        assert_eq!(
            check_envelope(ActionClass::ThirdParty, &envelope, &usage),
            EnvelopeVerdict::Allow
        );
    }

    #[test]
    fn a_subject_inside_its_cooldown_is_left_alone_by_every_play() {
        let envelope = GrowthEnvelope {
            subject_cooldown_hours: 168,
            ..running()
        };
        let usage = EnvelopeUsage {
            hours_since_subject_touched: Some(100),
            ..EnvelopeUsage::default()
        };
        assert_eq!(
            held(check_envelope(
                ActionClass::OwnedAudience,
                &envelope,
                &usage
            )),
            EnvelopeBlock::SubjectInCooldown {
                hours_remaining: 68
            }
        );
        let usage = EnvelopeUsage {
            hours_since_subject_touched: Some(168),
            ..EnvelopeUsage::default()
        };
        assert_eq!(
            check_envelope(ActionClass::OwnedAudience, &envelope, &usage),
            EnvelopeVerdict::Allow
        );
    }

    #[test]
    fn a_subject_never_touched_is_not_in_cooldown() {
        assert_eq!(
            check_envelope(
                ActionClass::OwnedAudience,
                &running(),
                &EnvelopeUsage::default()
            ),
            EnvelopeVerdict::Allow
        );
    }

    #[test]
    fn the_switch_outranks_the_budget_and_the_budget_outranks_the_cooldown() {
        // The order is what an operator would expect to be told first.
        let usage = EnvelopeUsage {
            owned_audience_touches_7d: 10_000,
            third_party_touches_7d: 10_000,
            third_party_touches_24h: 0,
            hours_since_subject_touched: Some(0),
            approval_requests_7d: 0,
        };
        assert_eq!(
            held(check_envelope(
                ActionClass::OwnedAudience,
                &GrowthEnvelope::default(),
                &usage
            )),
            EnvelopeBlock::AgentDisabled
        );
        assert!(matches!(
            held(check_envelope(
                ActionClass::OwnedAudience,
                &running(),
                &usage
            )),
            EnvelopeBlock::WeeklyBudgetExhausted { .. }
        ));
    }

    #[test]
    fn free_first_party_work_is_never_budgeted() {
        let envelope = GrowthEnvelope::default();
        assert_eq!(
            envelope.weekly_budget(ActionClass::FirstPartyReversible),
            None
        );
        // Spend is capped by money, not by a message budget.
        assert_eq!(envelope.weekly_budget(ActionClass::Paid), None);
        assert_eq!(
            EnvelopeUsage {
                owned_audience_touches_7d: 99,
                ..EnvelopeUsage::default()
            }
            .spent(ActionClass::FirstPartyReversible),
            0
        );
    }

    #[test]
    fn asking_has_room_until_the_weekly_budget_is_spent() {
        let envelope = GrowthEnvelope::default();
        let budget = envelope.weekly_approval_requests;
        assert!(budget > 0, "the default posture may ask");

        let under = EnvelopeUsage {
            approval_requests_7d: budget - 1,
            ..EnvelopeUsage::default()
        };
        assert_eq!(check_attention(&envelope, &under), AttentionVerdict::Ask);

        let at = EnvelopeUsage {
            approval_requests_7d: budget,
            ..EnvelopeUsage::default()
        };
        assert_eq!(
            check_attention(&envelope, &at),
            AttentionVerdict::Exhausted {
                spent: budget,
                budget
            }
        );
    }

    /// Asking is bounded whatever the action costs. A first-party action is
    /// free to the audience and not free to the person being asked about it,
    /// which is the whole reason this is not part of `check_envelope`.
    #[test]
    fn the_attention_budget_does_not_care_which_class_asked() {
        let envelope = GrowthEnvelope {
            weekly_approval_requests: 0,
            ..GrowthEnvelope::default()
        };
        let usage = EnvelopeUsage::default();
        assert!(!check_attention(&envelope, &usage).may_ask());
        // ...while the envelope itself still waves first-party work straight
        // through, because it reaches nobody.
        assert_eq!(
            check_envelope(ActionClass::FirstPartyReversible, &envelope, &usage),
            EnvelopeVerdict::Allow
        );
    }

    /// A budget of zero is a posture, not a misconfiguration: read the board,
    /// send me nothing.
    #[test]
    fn a_zero_budget_never_asks() {
        let envelope = GrowthEnvelope {
            weekly_approval_requests: 0,
            ..GrowthEnvelope::default()
        };
        assert_eq!(
            check_attention(&envelope, &EnvelopeUsage::default()),
            AttentionVerdict::Exhausted {
                spent: 0,
                budget: 0
            }
        );
    }

    #[test]
    fn every_block_names_itself() {
        for block in [
            EnvelopeBlock::AgentDisabled,
            EnvelopeBlock::DryRun,
            EnvelopeBlock::WeeklyBudgetExhausted {
                spent: 1,
                budget: 1,
            },
            EnvelopeBlock::SubjectInCooldown { hours_remaining: 1 },
        ] {
            assert!(!block.as_str().is_empty());
        }
    }
}
