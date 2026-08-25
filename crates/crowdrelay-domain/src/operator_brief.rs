//! What the operator is told, once a day, without being trained to ignore it.
//!
//! Every read model needed for a brief already exists; nothing has ever sent
//! one. That gap is not cosmetic. An agent that decides, parks work behind a
//! capability nobody advertises, and waits on approvals nobody sees is
//! indistinguishable from an agent with nothing to do — and the production
//! state this rule was written against was exactly that: the envelope off, a
//! dozen actions awaiting approval, and no one told.
//!
//! So the rule here is not "summarise the day". It is:
//!
//! - **Silence is the default.** A daily "nothing to report" is the fastest way
//!   to teach an operator to filter the brief into a folder they never open.
//! - **Except for the silences that lie.** An agent that is switched off while
//!   work piles up, or blocked on a human who does not know it, looks calm from
//!   the outside. Those two states break the silence rule, because they are the
//!   only ones where saying nothing is actively misleading.
//! - **One headline, not a digest.** The brief carries the single most
//!   important fact; the read models already serve the rest to anyone who asks.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

/// The state of the agent as of the brief, assembled from facts the control
/// and chief-of-staff read models already own.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct OperatorBriefSnapshot {
    /// Actions that actually completed, executor-confirmed where an executor
    /// was involved.
    pub actions_executed_24h: u32,
    pub actions_failed_24h: u32,
    /// Decisions the agent made and cannot carry out without a human.
    pub actions_awaiting_approval: u32,
    /// How long the oldest of those has been waiting. This is the number that
    /// turns a queue into a problem.
    pub oldest_approval_age_hours: Option<u32>,
    /// Actions parked because no executor advertises the capability they need.
    /// Work the agent decided on and physically cannot perform.
    pub actions_parked: u32,
    /// True when parked actions exist but no executor has a fresh heartbeat.
    /// The execution plane itself is dead, not just one capability gap.
    pub execution_plane_dead: bool,
    /// Off-platform feeds the agent has no series for. Reported so "we saw no
    /// change" is never confused with "we could not look".
    pub blind_platforms: u16,
    /// The most recent discovery sweep answered having read nothing at all.
    ///
    /// Separate from `blind_platforms` because it is a different loss: that one
    /// means the agent cannot measure, this one means it cannot find anybody
    /// new to reach. It is also the quietest failure the system has — the sweep
    /// succeeds, the batch is empty, and every downstream table stays at zero
    /// while the ledger reads green.
    pub last_sweep_read_nothing: bool,
    pub agent_enabled: bool,
    pub dry_run: bool,
    pub last_brief_at: Option<OffsetDateTime>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct OperatorBriefPolicy {
    /// A brief a day. Shorter turns the exception rules into a pager.
    pub send_interval_hours: u16,
    /// An approval older than this is the headline, whatever else happened.
    pub stale_approval_hours: u32,
}

impl Default for OperatorBriefPolicy {
    fn default() -> Self {
        Self {
            send_interval_hours: 24,
            stale_approval_hours: 48,
        }
    }
}

/// The one thing the brief leads with. Ordered by what costs the band most if
/// it goes unread for another day.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BriefHeadline {
    /// Routine: the agent worked and nothing needs a human.
    Worked,
    /// The agent cannot see platforms it is expected to report on.
    Blind,
    /// The last discovery sweep read nothing, so the pitcher's only source of
    /// new targets is producing none. Ranked above `Blind` because a broken
    /// read path stops the agent reaching anybody new, where a missing metric
    /// series only stops it measuring. Ranked below `Failing`: a failed action
    /// is already visible in the ledger, and this is not.
    DiscoveryReadNothing,
    /// Actions are parked AND no executor has heartbeated recently. The
    /// execution plane itself is dead, not just one capability gap.
    ExecutionPlaneDead,
    /// Actions failed.
    Failing,
    /// Decisions are waiting on a human inside the normal window.
    AwaitingApproval,
    /// The agent decided on work it physically cannot perform, because no
    /// executor advertises the capability. Nothing the operator does in the
    /// approval queue will move these.
    WorkParked,
    /// The agent is off, or in dry run, while there is work it would do. The
    /// most expensive silence there is: it looks exactly like a quiet week.
    DisabledWithWorkWaiting,
    /// A human has been sitting on a decision past the policy horizon.
    ApprovalStale,
}

impl BriefHeadline {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Worked => "worked",
            Self::Blind => "blind",
            Self::DiscoveryReadNothing => "discovery_read_nothing",
            Self::ExecutionPlaneDead => "execution_plane_dead",
            Self::Failing => "failing",
            Self::AwaitingApproval => "awaiting_approval",
            Self::WorkParked => "work_parked",
            Self::DisabledWithWorkWaiting => "disabled_with_work_waiting",
            Self::ApprovalStale => "approval_stale",
        }
    }

    /// Whether this headline is worth breaking the silence rule for. A brief
    /// that only ever says the agent worked is one nobody reads by week three.
    #[must_use]
    pub const fn warrants_interrupting(self) -> bool {
        !matches!(self, Self::Worked)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BriefHold {
    /// A brief was sent inside the interval.
    IntervalNotElapsed,
    /// Nothing happened and nothing is waiting. Saying so daily is how a brief
    /// becomes noise.
    NothingWorthSaying,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum OperatorBriefDecision {
    Send(BriefHeadline),
    Hold(BriefHold),
}

/// Decides whether to brief the operator, and what to lead with.
///
/// The interval is checked first and unconditionally: none of the headlines
/// below are alerts, and an agent that is off stays off, so re-sending the same
/// fact every cycle would make the one message a day worth ignoring.
#[must_use]
pub fn evaluate_operator_brief(
    snapshot: &OperatorBriefSnapshot,
    policy: OperatorBriefPolicy,
    now: OffsetDateTime,
) -> OperatorBriefDecision {
    if let Some(last) = snapshot.last_brief_at {
        let interval = Duration::hours(i64::from(policy.send_interval_hours));
        if now - last < interval {
            return OperatorBriefDecision::Hold(BriefHold::IntervalNotElapsed);
        }
    }
    let headline = headline(snapshot, policy);
    if headline.warrants_interrupting() || snapshot.actions_executed_24h > 0 {
        return OperatorBriefDecision::Send(headline);
    }
    OperatorBriefDecision::Hold(BriefHold::NothingWorthSaying)
}

/// The single fact the brief leads with, most expensive first.
fn headline(snapshot: &OperatorBriefSnapshot, policy: OperatorBriefPolicy) -> BriefHeadline {
    let waiting = snapshot.actions_awaiting_approval + snapshot.actions_parked;
    if snapshot
        .oldest_approval_age_hours
        .is_some_and(|hours| hours >= policy.stale_approval_hours)
    {
        return BriefHeadline::ApprovalStale;
    }
    // Being switched off only matters when there is something to be off for.
    // A disabled agent with an empty queue is a decision, not a problem.
    if (!snapshot.agent_enabled || snapshot.dry_run) && waiting > 0 {
        return BriefHeadline::DisabledWithWorkWaiting;
    }
    if snapshot.actions_parked > 0 && snapshot.execution_plane_dead {
        return BriefHeadline::ExecutionPlaneDead;
    }
    if snapshot.actions_parked > 0 {
        return BriefHeadline::WorkParked;
    }
    if snapshot.actions_awaiting_approval > 0 {
        return BriefHeadline::AwaitingApproval;
    }
    if snapshot.actions_failed_24h > 0 {
        return BriefHeadline::Failing;
    }
    if snapshot.last_sweep_read_nothing {
        return BriefHeadline::DiscoveryReadNothing;
    }
    if snapshot.blind_platforms > 0 {
        return BriefHeadline::Blind;
    }
    BriefHeadline::Worked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_780_000_000).expect("valid timestamp")
    }

    fn quiet() -> OperatorBriefSnapshot {
        OperatorBriefSnapshot {
            actions_executed_24h: 0,
            actions_failed_24h: 0,
            actions_awaiting_approval: 0,
            oldest_approval_age_hours: None,
            actions_parked: 0,
            execution_plane_dead: false,
            blind_platforms: 0,
            last_sweep_read_nothing: false,
            agent_enabled: true,
            dry_run: false,
            last_brief_at: None,
        }
    }

    fn sent(decision: OperatorBriefDecision) -> BriefHeadline {
        match decision {
            OperatorBriefDecision::Send(headline) => headline,
            OperatorBriefDecision::Hold(hold) => panic!("expected a brief, held as {hold:?}"),
        }
    }

    #[test]
    fn a_quiet_day_with_nothing_waiting_says_nothing() {
        assert_eq!(
            evaluate_operator_brief(&quiet(), OperatorBriefPolicy::default(), now()),
            OperatorBriefDecision::Hold(BriefHold::NothingWorthSaying)
        );
    }

    #[test]
    fn a_day_the_agent_worked_is_reported_even_though_nothing_is_wrong() {
        let snapshot = OperatorBriefSnapshot {
            actions_executed_24h: 4,
            ..quiet()
        };
        assert_eq!(
            sent(evaluate_operator_brief(
                &snapshot,
                OperatorBriefPolicy::default(),
                now()
            )),
            BriefHeadline::Worked
        );
    }

    #[test]
    fn an_agent_switched_off_with_work_waiting_is_the_silence_that_must_break() {
        // The production state this rule was written against: envelope off,
        // decisions piling up, and nothing distinguishing it from a quiet week.
        let snapshot = OperatorBriefSnapshot {
            agent_enabled: false,
            dry_run: true,
            actions_awaiting_approval: 12,
            ..quiet()
        };
        assert_eq!(
            sent(evaluate_operator_brief(
                &snapshot,
                OperatorBriefPolicy::default(),
                now()
            )),
            BriefHeadline::DisabledWithWorkWaiting
        );
    }

    #[test]
    fn an_agent_switched_off_with_an_empty_queue_is_a_decision_not_a_problem() {
        let snapshot = OperatorBriefSnapshot {
            agent_enabled: false,
            dry_run: true,
            ..quiet()
        };
        assert_eq!(
            evaluate_operator_brief(&snapshot, OperatorBriefPolicy::default(), now()),
            OperatorBriefDecision::Hold(BriefHold::NothingWorthSaying)
        );
    }

    #[test]
    fn a_stale_approval_outranks_every_other_headline() {
        let snapshot = OperatorBriefSnapshot {
            agent_enabled: false,
            dry_run: true,
            actions_awaiting_approval: 3,
            actions_parked: 5,
            actions_failed_24h: 9,
            blind_platforms: 3,
            oldest_approval_age_hours: Some(72),
            ..quiet()
        };
        assert_eq!(
            sent(evaluate_operator_brief(
                &snapshot,
                OperatorBriefPolicy::default(),
                now()
            )),
            BriefHeadline::ApprovalStale
        );
    }

    #[test]
    fn parked_work_outranks_an_approval_queue_a_human_can_actually_clear() {
        // Approving a parked action changes nothing: no executor advertises the
        // capability. Leading with the queue would send the operator to work
        // that cannot move.
        let snapshot = OperatorBriefSnapshot {
            actions_awaiting_approval: 8,
            actions_parked: 1,
            ..quiet()
        };
        assert_eq!(
            sent(evaluate_operator_brief(
                &snapshot,
                OperatorBriefPolicy::default(),
                now()
            )),
            BriefHeadline::WorkParked
        );
    }

    #[test]
    fn failures_outrank_blindness_because_one_is_happening_now() {
        let snapshot = OperatorBriefSnapshot {
            actions_failed_24h: 2,
            blind_platforms: 3,
            ..quiet()
        };
        assert_eq!(
            sent(evaluate_operator_brief(
                &snapshot,
                OperatorBriefPolicy::default(),
                now()
            )),
            BriefHeadline::Failing
        );
    }

    #[test]
    fn platforms_the_agent_cannot_see_are_reported_rather_than_read_as_quiet() {
        let snapshot = OperatorBriefSnapshot {
            blind_platforms: 3,
            ..quiet()
        };
        assert_eq!(
            sent(evaluate_operator_brief(
                &snapshot,
                OperatorBriefPolicy::default(),
                now()
            )),
            BriefHeadline::Blind
        );
    }

    #[test]
    fn a_sweep_that_read_nothing_outranks_a_missing_metric_feed() {
        // Both are blindness, and they are not equally expensive. A missing
        // series stops the agent measuring; a sweep that reads nothing stops it
        // finding anybody new to reach, which is the growth loop itself.
        let snapshot = OperatorBriefSnapshot {
            blind_platforms: 3,
            last_sweep_read_nothing: true,
            ..quiet()
        };
        assert_eq!(
            sent(evaluate_operator_brief(
                &snapshot,
                OperatorBriefPolicy::default(),
                now()
            )),
            BriefHeadline::DiscoveryReadNothing
        );
    }

    #[test]
    fn a_failed_action_still_outranks_a_sweep_that_read_nothing() {
        let snapshot = OperatorBriefSnapshot {
            actions_failed_24h: 1,
            last_sweep_read_nothing: true,
            ..quiet()
        };
        assert_eq!(
            sent(evaluate_operator_brief(
                &snapshot,
                OperatorBriefPolicy::default(),
                now()
            )),
            BriefHeadline::Failing
        );
    }

    #[test]
    fn a_sweep_that_read_nothing_breaks_the_silence_on_its_own() {
        // Nothing executed, nothing failed, nothing waiting. Without this the
        // brief would hold as `NothingWorthSaying` and the broken read path
        // would stay invisible for exactly as long as nobody looked.
        let snapshot = OperatorBriefSnapshot {
            last_sweep_read_nothing: true,
            ..quiet()
        };
        assert!(matches!(
            evaluate_operator_brief(&snapshot, OperatorBriefPolicy::default(), now()),
            OperatorBriefDecision::Send(BriefHeadline::DiscoveryReadNothing)
        ));
    }

    #[test]
    fn nothing_is_sent_twice_inside_the_interval_however_bad_the_news() {
        let snapshot = OperatorBriefSnapshot {
            agent_enabled: false,
            actions_awaiting_approval: 40,
            oldest_approval_age_hours: Some(200),
            last_brief_at: Some(now() - Duration::hours(2)),
            ..quiet()
        };
        assert_eq!(
            evaluate_operator_brief(&snapshot, OperatorBriefPolicy::default(), now()),
            OperatorBriefDecision::Hold(BriefHold::IntervalNotElapsed)
        );
        let elapsed = OperatorBriefSnapshot {
            last_brief_at: Some(now() - Duration::hours(25)),
            ..snapshot
        };
        assert!(matches!(
            evaluate_operator_brief(&elapsed, OperatorBriefPolicy::default(), now()),
            OperatorBriefDecision::Send(_)
        ));
    }

    #[test]
    fn headline_ordering_matches_the_cost_of_leaving_it_unread() {
        // The enum order is load-bearing: it is what a reader compares against
        // when adding a headline later.
        let mut ordered = [
            BriefHeadline::ApprovalStale,
            BriefHeadline::Worked,
            BriefHeadline::WorkParked,
            BriefHeadline::Blind,
            BriefHeadline::DisabledWithWorkWaiting,
            BriefHeadline::Failing,
            BriefHeadline::DiscoveryReadNothing,
            BriefHeadline::AwaitingApproval,
        ];
        ordered.sort_unstable();
        assert_eq!(
            ordered,
            [
                BriefHeadline::Worked,
                BriefHeadline::Blind,
                BriefHeadline::DiscoveryReadNothing,
                BriefHeadline::Failing,
                BriefHeadline::AwaitingApproval,
                BriefHeadline::WorkParked,
                BriefHeadline::DisabledWithWorkWaiting,
                BriefHeadline::ApprovalStale,
            ]
        );
        assert!(!BriefHeadline::Worked.warrants_interrupting());
        for headline in [
            BriefHeadline::Blind,
            BriefHeadline::Failing,
            BriefHeadline::AwaitingApproval,
            BriefHeadline::WorkParked,
            BriefHeadline::DisabledWithWorkWaiting,
            BriefHeadline::ApprovalStale,
        ] {
            assert!(headline.warrants_interrupting());
        }
    }
}
