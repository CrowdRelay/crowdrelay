//! What the agent has learned about its own plays, and what it is allowed to do
//! with that.
//!
//! Everything before this phase decides from the present: a series moved, a
//! step is due, a pipeline is empty. Nothing carried a memory of whether a kind
//! of campaign has ever worked, so a play that measured `worsened` three times
//! running was proposed exactly as often as one that measured `improved`.
//!
//! Four rules bound what a record may change.
//!
//! 1. **An unmeasurable outcome is not a bad outcome.** A play whose claim
//!    settled `insufficient` counts neither for nor against. Letting it count
//!    would retire the plays the agent cannot see rather than the ones that do
//!    not work — and being unable to measure something is a reason to fix the
//!    measurement, not to stop acting.
//! 2. **One result changes nothing.** Below a minimum measured record the
//!    standing is [`PlayStanding::Untested`] and the play runs at full reach.
//!    A single bad fortnight is noise.
//! 3. **A record may only ever narrow.** The weight caps at full and scales the
//!    play's recipient ceiling downward; there is no number of good results that
//!    widens anything. Authority — the context ladder, the class ceiling, the
//!    envelope — is untouched by this module and moves only when a human moves
//!    it, however good the record looks.
//! 4. **Retirement is a stated fact, not a decayed weight.** A play retires on a
//!    run of measured `worsened` outcomes, with the reason recorded, and comes
//!    back only when an operator says so.

use serde::{Deserialize, Serialize};

use crate::performance::EffectAssessment;

/// What the measured record says about one kind of play.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "standing", rename_all = "snake_case")]
pub enum PlayStanding {
    /// Too few measured outcomes to say anything. Runs at full reach: the
    /// alternative is an agent that throttles every new play before it has had
    /// a chance to work.
    Untested { measured: u32 },
    /// Weighted by its own record. `10_000` is full reach; lower means the play
    /// reaches proportionally fewer people until the record improves.
    Weighted { basis_points: u16, measured: u32 },
    /// Proposed no longer, until an operator reinstates it.
    Retired { reason: RetirementReason },
}

impl PlayStanding {
    /// `10_000` when nothing is holding the play back, and zero when it is
    /// retired.
    #[must_use]
    pub const fn weight_basis_points(self) -> u16 {
        match self {
            Self::Untested { .. } => 10_000,
            Self::Weighted { basis_points, .. } => basis_points,
            Self::Retired { .. } => 0,
        }
    }

    #[must_use]
    pub const fn is_retired(self) -> bool {
        matches!(self, Self::Retired { .. })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetirementReason {
    /// A run of measured outcomes that each made the success metric worse.
    RepeatedlyWorsened,
    /// A human switched it off. Recorded separately so the agent never claims
    /// an operator's decision as its own conclusion.
    OperatorRetired,
}

impl RetirementReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RepeatedlyWorsened => "repeatedly_worsened",
            Self::OperatorRetired => "operator_retired",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "repeatedly_worsened" => Some(Self::RepeatedlyWorsened),
            "operator_retired" => Some(Self::OperatorRetired),
            _ => None,
        }
    }
}

/// The record one kind of play has accumulated.
///
/// Counts rather than a score, because a score cannot be argued with. An
/// operator who disagrees with a standing can see exactly which outcomes
/// produced it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlayRecord {
    pub improved: u32,
    pub neutral: u32,
    pub worsened: u32,
    /// Outcomes that could not be measured. Carried so the record is complete,
    /// and deliberately absent from every calculation below.
    pub insufficient: u32,
    /// Measured `worsened` outcomes since the last one that was not. Reset by
    /// any measured result that is not `worsened`; untouched by an unmeasurable
    /// one, which is neither a recovery nor a further failure.
    pub consecutive_worsened: u32,
    /// Set only by an operator. The agent never writes this.
    pub operator_retired: bool,
}

impl PlayRecord {
    /// Outcomes that actually said something.
    #[must_use]
    pub const fn measured(self) -> u32 {
        self.improved
            .saturating_add(self.neutral)
            .saturating_add(self.worsened)
    }

    /// Folds one settled outcome in. `None` is an unmeasurable claim.
    #[must_use]
    pub const fn observe(self, assessment: Option<EffectAssessment>) -> Self {
        match assessment {
            Some(EffectAssessment::Improved) => Self {
                improved: self.improved.saturating_add(1),
                consecutive_worsened: 0,
                ..self
            },
            Some(EffectAssessment::Neutral) => Self {
                neutral: self.neutral.saturating_add(1),
                consecutive_worsened: 0,
                ..self
            },
            Some(EffectAssessment::Worsened) => Self {
                worsened: self.worsened.saturating_add(1),
                consecutive_worsened: self.consecutive_worsened.saturating_add(1),
                ..self
            },
            // Neither evidence for nor against. The run of failures is not
            // broken by a result nobody could read, and not extended by one
            // either.
            None => Self {
                insufficient: self.insufficient.saturating_add(1),
                ..self
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct LearningPolicy {
    /// Measured outcomes required before the record moves anything at all.
    pub minimum_measured_record: u32,
    /// However bad the record, a play that is still running reaches at least
    /// this share of its ceiling. A weight that could fall to nothing would be
    /// a silent retirement — and retirement is meant to be a stated fact.
    pub floor_basis_points: u16,
    /// Consecutive measured `worsened` outcomes before the play retires itself.
    pub retire_after_consecutive_worsened: u32,
}

impl Default for LearningPolicy {
    fn default() -> Self {
        Self {
            // Three is enough to distinguish a run from an accident and few
            // enough that a bad play is not run twenty times to prove it.
            minimum_measured_record: 3,
            floor_basis_points: 2_500,
            retire_after_consecutive_worsened: 3,
        }
    }
}

/// Turns a record into a standing.
#[must_use]
pub fn assess_play_standing(record: PlayRecord, policy: LearningPolicy) -> PlayStanding {
    if record.operator_retired {
        return PlayStanding::Retired {
            reason: RetirementReason::OperatorRetired,
        };
    }
    // Checked before the sample-size guard on purpose. A play that has made the
    // number worse every time it was measured does not get to keep running
    // because it has not been measured often enough yet.
    if record.consecutive_worsened >= policy.retire_after_consecutive_worsened.max(1) {
        return PlayStanding::Retired {
            reason: RetirementReason::RepeatedlyWorsened,
        };
    }
    let measured = record.measured();
    if measured < policy.minimum_measured_record.max(1) {
        return PlayStanding::Untested { measured };
    }
    // Neutral counts as half. A play that reliably does nothing is not as good
    // as one that works and not as bad as one that harms, and scoring it as
    // either would be a claim the record does not support.
    let credit = u64::from(record.improved)
        .saturating_mul(2)
        .saturating_add(u64::from(record.neutral));
    let basis_points = credit.saturating_mul(10_000) / u64::from(measured).max(1) / 2;
    let basis_points = u16::try_from(basis_points.min(10_000)).unwrap_or(10_000);
    PlayStanding::Weighted {
        basis_points: basis_points.max(policy.floor_basis_points.min(10_000)),
        measured,
    }
}

/// How many recipients one step of this play may reach, given its standing.
///
/// The only thing a record is allowed to change. It scales the operator's own
/// ceiling downward and can never raise it: a play with a perfect record still
/// reaches exactly the number an operator configured.
#[must_use]
pub fn effective_recipient_ceiling(max_recipients_per_step: u32, standing: PlayStanding) -> u32 {
    if standing.is_retired() {
        return 0;
    }
    // An operator who configured nothing gets nothing. The floor below exists so
    // a weight cannot become a silent retirement, not so a record can overrule a
    // deliberate zero — raising it here would be this module doing the one thing
    // it is not allowed to do.
    if max_recipients_per_step == 0 {
        return 0;
    }
    let scaled = u64::from(max_recipients_per_step)
        .saturating_mul(u64::from(standing.weight_basis_points()))
        / 10_000;
    // A running play always reaches somebody. Nought recipients is retirement
    // wearing a weight's clothes, and it would hide from every read model that
    // reports why a play stopped.
    u32::try_from(scaled)
        .unwrap_or(max_recipients_per_step)
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> LearningPolicy {
        LearningPolicy::default()
    }

    fn record(improved: u32, neutral: u32, worsened: u32) -> PlayRecord {
        PlayRecord {
            improved,
            neutral,
            worsened,
            ..PlayRecord::default()
        }
    }

    #[test]
    fn a_play_nobody_has_measured_runs_at_full_reach() {
        let standing = assess_play_standing(record(1, 0, 0), policy());
        assert!(matches!(standing, PlayStanding::Untested { measured: 1 }));
        assert_eq!(standing.weight_basis_points(), 10_000);
        assert_eq!(effective_recipient_ceiling(150, standing), 150);
    }

    #[test]
    fn an_unmeasurable_outcome_counts_neither_way() {
        let observed = PlayRecord::default()
            .observe(None)
            .observe(None)
            .observe(None)
            .observe(None);
        assert_eq!(observed.measured(), 0);
        assert_eq!(observed.insufficient, 4);
        assert!(matches!(
            assess_play_standing(observed, policy()),
            PlayStanding::Untested { measured: 0 }
        ));
    }

    #[test]
    fn an_unmeasurable_outcome_neither_breaks_nor_extends_a_run_of_failures() {
        let observed = PlayRecord::default()
            .observe(Some(EffectAssessment::Worsened))
            .observe(None)
            .observe(Some(EffectAssessment::Worsened));
        assert_eq!(observed.consecutive_worsened, 2);
        let recovered = observed.observe(Some(EffectAssessment::Neutral));
        assert_eq!(recovered.consecutive_worsened, 0);
    }

    #[test]
    fn a_play_that_keeps_making_the_number_worse_retires_itself() {
        let mut observed = PlayRecord::default();
        for _ in 0..3 {
            observed = observed.observe(Some(EffectAssessment::Worsened));
        }
        assert_eq!(
            assess_play_standing(observed, policy()),
            PlayStanding::Retired {
                reason: RetirementReason::RepeatedlyWorsened
            }
        );
        assert_eq!(
            effective_recipient_ceiling(
                150,
                PlayStanding::Retired {
                    reason: RetirementReason::RepeatedlyWorsened
                }
            ),
            0
        );
    }

    #[test]
    fn one_bad_result_changes_nothing() {
        let standing = assess_play_standing(record(0, 0, 1), policy());
        assert!(matches!(standing, PlayStanding::Untested { .. }));
        assert_eq!(standing.weight_basis_points(), 10_000);
    }

    #[test]
    fn a_mixed_record_narrows_reach_without_stopping_the_play() {
        let standing = assess_play_standing(record(1, 1, 1), policy());
        let PlayStanding::Weighted { basis_points, .. } = standing else {
            panic!("three measured outcomes are a record");
        };
        assert!(basis_points < 10_000, "a mixed record is not a full record");
        assert!(basis_points >= policy().floor_basis_points);
        let reach = effective_recipient_ceiling(150, standing);
        assert!(reach > 0 && reach < 150);
    }

    #[test]
    fn a_perfect_record_never_widens_anything() {
        let standing = assess_play_standing(record(20, 0, 0), policy());
        assert_eq!(standing.weight_basis_points(), 10_000);
        assert_eq!(
            effective_recipient_ceiling(150, standing),
            150,
            "however good the record, the operator's ceiling is the ceiling"
        );
    }

    #[test]
    fn a_running_play_always_reaches_somebody() {
        // The worst survivable record still sends to at least one fan, so a
        // play that stopped is always a retirement somebody can read.
        let standing = assess_play_standing(
            record(0, 0, 2).observe(Some(EffectAssessment::Neutral)),
            policy(),
        );
        assert!(effective_recipient_ceiling(1, standing) >= 1);
    }

    #[test]
    fn a_configured_zero_is_never_raised_to_one() {
        // The floor exists so a weight cannot become a silent retirement, not
        // so a record can overrule an operator who deliberately set nothing.
        let silent = 0u32;
        for standing in [
            PlayStanding::Untested { measured: 0 },
            PlayStanding::Weighted {
                basis_points: 10_000,
                measured: 9,
            },
        ] {
            assert_eq!(effective_recipient_ceiling(silent, standing), 0);
        }
    }

    #[test]
    fn an_operator_retirement_is_never_claimed_as_the_agents_conclusion() {
        let observed = PlayRecord {
            operator_retired: true,
            ..record(9, 0, 0)
        };
        assert_eq!(
            assess_play_standing(observed, policy()),
            PlayStanding::Retired {
                reason: RetirementReason::OperatorRetired
            }
        );
        for reason in [
            RetirementReason::RepeatedlyWorsened,
            RetirementReason::OperatorRetired,
        ] {
            assert_eq!(RetirementReason::parse(reason.as_str()), Some(reason));
        }
    }
}
