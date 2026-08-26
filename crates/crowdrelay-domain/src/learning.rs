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

// ---------------------------------------------------------------------------
// Outreach kind learning
//
// The same discipline as play learning, applied to outreach target kinds.
// A wave of playlist pitches that measures `worsened` three times running is
// proposed less, and eventually retires itself. The weight scales one number
// — how many pitches a wave of this kind may carry — and only downward.
// ---------------------------------------------------------------------------

/// The record one kind of outreach target has accumulated. Identical in shape
/// to [`PlayRecord`] because the discipline is the same: counts not a score,
/// `insufficient` carried but never counted, and a run of `worsened` that
/// resets on any measured result that is not.
pub type OutreachKindRecord = PlayRecord;

/// The standing of one kind of outreach target. Same shape as [`PlayStanding`]
/// for the same reasons.
pub type OutreachKindStanding = PlayStanding;

/// How many pitches a wave of this kind may carry, given its standing.
///
/// The only thing a record is allowed to change. It scales the operator's own
/// wave-size ceiling downward and can never raise it.
#[must_use]
pub fn effective_wave_ceiling(max_pitches_per_wave: u32, standing: OutreachKindStanding) -> u32 {
    effective_recipient_ceiling(max_pitches_per_wave, standing)
}

// ---------------------------------------------------------------------------
// Wave outcome assessment
//
// A wave's effect is simpler than a play's: no metric series, no baseline, no
// trend. The targets replied or they did not, and what they said is already
// classified. The assessment turns those classified replies into the same
// `EffectAssessment` the play learning record consumes.
// ---------------------------------------------------------------------------

/// The raw counts a wave outcome worker reads from the reply table.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WaveReplyCounts {
    pub positive: u32,
    pub declined: u32,
    pub do_not_contact: u32,
    pub total: u32,
}

/// The verdict for one wave: measured (with an assessment) or insufficient.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaveOutcomeVerdict {
    /// Enough replies to judge the kind. The assessment folds into the record.
    Measured { assessment: EffectAssessment },
    /// Not enough replies to judge. Folds into the record as `insufficient` —
    /// counts neither for nor against, same as a play whose metric could not be
    /// read.
    Insufficient { reason: InsufficientReason },
}

/// Why a wave's outcome could not be measured.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InsufficientReason {
    /// No replies at all. Being ignored is a reason to fix the pitch, not to
    /// retire the kind.
    NoReplies,
    /// Too few replies relative to pitches sent. Two replies from thirty
    /// pitches is noise, not signal.
    BelowQuorum,
}

impl InsufficientReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoReplies => "no_replies",
            Self::BelowQuorum => "below_quorum",
        }
    }
}

/// The minimum fraction of pitches that must reply for the outcome to count.
/// Below this the verdict is `Insufficient::BelowQuorum`.
const REPLY_QUORUM_BASIS_POINTS: u32 = 2000; // 20%

/// Assesses a wave's reply counts into a verdict.
///
/// Rules:
/// - Zero replies → `Insufficient::NoReplies`.
/// - Replies below the quorum (20% of pitches sent) → `Insufficient::BelowQuorum`.
/// - Otherwise `Measured`: `Improved` if positive replies outnumber
///   `do_not_contact` by 2:1 or more, `Worsened` if `do_not_contact` outnumbers
///   positive by 2:1 or more, `Neutral` otherwise.
///
/// The 2:1 ratio stops a wave with 3 positive and 2 declined from counting as
/// improved — that is noise, not signal. A `do_not_contact` is weighted harder
/// than a decline because it is a request to stop, not a refusal of one pitch.
#[must_use]
pub fn assess_wave_outcome(counts: WaveReplyCounts, pitches_sent: u32) -> WaveOutcomeVerdict {
    if counts.total == 0 {
        return WaveOutcomeVerdict::Insufficient {
            reason: InsufficientReason::NoReplies,
        };
    }
    // The quorum is a fraction of pitches sent, not a fixed number. A wave of
    // 3 needs 1 reply; a wave of 30 needs 6.
    let quorum = (u64::from(pitches_sent) * u64::from(REPLY_QUORUM_BASIS_POINTS) / 10_000)
        .min(u64::from(u32::MAX)) as u32;
    if counts.total < quorum.max(1) {
        return WaveOutcomeVerdict::Insufficient {
            reason: InsufficientReason::BelowQuorum,
        };
    }
    // do_not_contact counts double: it is a request to stop, not just a "no".
    let weighted_negative = u64::from(counts.declined) + u64::from(counts.do_not_contact) * 2;
    let positive = u64::from(counts.positive);
    let assessment = if positive >= weighted_negative * 2 && positive > 0 {
        EffectAssessment::Improved
    } else if weighted_negative >= positive * 2 && weighted_negative > 0 {
        EffectAssessment::Worsened
    } else {
        EffectAssessment::Neutral
    };
    WaveOutcomeVerdict::Measured { assessment }
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

    // -------------------------------------------------------------------------
    // Outreach kind learning — same discipline, keyed on OutreachTargetKind.
    // -------------------------------------------------------------------------

    #[test]
    fn an_outreach_kind_with_no_record_runs_at_full_wave_size() {
        let standing = assess_play_standing(OutreachKindRecord::default(), policy());
        assert_eq!(standing.weight_basis_points(), 10_000);
        assert_eq!(effective_wave_ceiling(50, standing), 50);
    }

    #[test]
    fn a_worsened_outreach_kind_record_narrows_the_wave_ceiling() {
        let record = OutreachKindRecord {
            improved: 0,
            neutral: 1,
            worsened: 4,
            ..OutreachKindRecord::default()
        };
        let standing = assess_play_standing(record, policy());
        // Below 10_000 but above the floor (2_500).
        assert!(standing.weight_basis_points() < 10_000);
        assert!(standing.weight_basis_points() >= 2_500);
        let ceiling = effective_wave_ceiling(50, standing);
        assert!(ceiling < 50, "a bad record narrows the wave");
        assert!(ceiling >= 1, "but never to nothing");
    }

    #[test]
    fn a_retired_outreach_kind_reaches_zero_pitches() {
        let record = OutreachKindRecord {
            worsened: 3,
            consecutive_worsened: 3,
            ..OutreachKindRecord::default()
        };
        let standing = assess_play_standing(record, policy());
        assert!(standing.is_retired());
        assert_eq!(effective_wave_ceiling(50, standing), 0);
    }

    #[test]
    fn an_operator_zero_wave_size_stays_zero_regardless_of_record() {
        // A configured zero is a deliberate choice, not a floor to override.
        let standing = PlayStanding::Untested { measured: 0 };
        assert_eq!(effective_wave_ceiling(0, standing), 0);
    }

    #[test]
    fn every_outreach_target_kind_round_trips_through_parse() {
        for kind in [
            crate::outreach::OutreachTargetKind::Playlist,
            crate::outreach::OutreachTargetKind::Radio,
            crate::outreach::OutreachTargetKind::Press,
            crate::outreach::OutreachTargetKind::Creator,
            crate::outreach::OutreachTargetKind::SupportSlot,
            crate::outreach::OutreachTargetKind::Endorsement,
            crate::outreach::OutreachTargetKind::MediaPatronage,
        ] {
            assert_eq!(
                crate::outreach::OutreachTargetKind::parse(kind.as_str()),
                Some(kind)
            );
        }
        assert_eq!(crate::outreach::OutreachTargetKind::all().len(), 7);
    }

    // ---------------------------------------------------------------------
    // Wave outcome assessment
    // ---------------------------------------------------------------------

    fn counts(positive: u32, declined: u32, dnc: u32) -> WaveReplyCounts {
        WaveReplyCounts {
            positive,
            declined,
            do_not_contact: dnc,
            total: positive + declined + dnc,
        }
    }

    #[test]
    fn a_wave_with_no_replies_is_insufficient_not_worsened() {
        let verdict = assess_wave_outcome(counts(0, 0, 0), 10);
        assert_eq!(
            verdict,
            WaveOutcomeVerdict::Insufficient {
                reason: InsufficientReason::NoReplies
            }
        );
    }

    #[test]
    fn a_wave_below_quorum_is_insufficient() {
        // 1 reply from 30 pitches is 3.3% — below the 20% quorum.
        let verdict = assess_wave_outcome(counts(1, 0, 0), 30);
        assert_eq!(
            verdict,
            WaveOutcomeVerdict::Insufficient {
                reason: InsufficientReason::BelowQuorum
            }
        );
    }

    #[test]
    fn a_wave_at_quorum_is_measured() {
        // 6 replies from 30 pitches is 20% — at the quorum.
        let verdict = assess_wave_outcome(counts(6, 0, 0), 30);
        assert!(matches!(verdict, WaveOutcomeVerdict::Measured { .. }));
    }

    #[test]
    fn a_wave_with_strong_positive_replies_is_improved() {
        // 10 positive, 2 declined, 0 dnc: positive outnumbers weighted_negative
        // (2) by 5:1, which is above 2:1.
        let verdict = assess_wave_outcome(counts(10, 2, 0), 30);
        assert_eq!(
            verdict,
            WaveOutcomeVerdict::Measured {
                assessment: EffectAssessment::Improved
            }
        );
    }

    #[test]
    fn a_wave_with_do_not_contact_replies_is_worsened() {
        // do_not_contact counts double: 0 positive, 0 declined, 7 dnc = 14
        // weighted negative, which outnumbers 0 positive by more than 2:1.
        // 7 replies from 30 pitches = 23%, above the 20% quorum.
        let verdict = assess_wave_outcome(counts(0, 0, 7), 30);
        assert_eq!(
            verdict,
            WaveOutcomeVerdict::Measured {
                assessment: EffectAssessment::Worsened
            }
        );
    }

    #[test]
    fn a_wave_with_balanced_replies_is_neutral() {
        // 5 positive, 3 declined, 1 dnc: weighted_negative = 3 + 2 = 5.
        // positive (5) vs weighted_negative (5) is 1:1, not 2:1 either way.
        let verdict = assess_wave_outcome(counts(5, 3, 1), 30);
        assert_eq!(
            verdict,
            WaveOutcomeVerdict::Measured {
                assessment: EffectAssessment::Neutral
            }
        );
    }

    #[test]
    fn do_not_contact_counts_double_against_the_kind() {
        // 4 positive, 0 declined, 2 dnc: weighted_negative = 0 + 4 = 4.
        // positive (4) vs weighted_negative (4) is 1:1 — neutral, not improved.
        // Without the double weight this would be 4:2 = 2:1 = improved.
        let verdict = assess_wave_outcome(counts(4, 0, 2), 30);
        assert_eq!(
            verdict,
            WaveOutcomeVerdict::Measured {
                assessment: EffectAssessment::Neutral
            }
        );
    }

    #[test]
    fn a_small_wave_needs_only_one_reply_to_be_measured() {
        // 3 pitches, quorum = max(3 * 20 / 100, 1) = max(0, 1) = 1.
        // 1 reply meets the quorum.
        let verdict = assess_wave_outcome(counts(1, 0, 0), 3);
        assert!(matches!(verdict, WaveOutcomeVerdict::Measured { .. }));
    }

    #[test]
    fn insufficient_reason_round_trips() {
        for reason in [
            InsufficientReason::NoReplies,
            InsufficientReason::BelowQuorum,
        ] {
            assert_eq!(reason.as_str(), reason.as_str());
        }
        assert_eq!(InsufficientReason::NoReplies.as_str(), "no_replies");
        assert_eq!(InsufficientReason::BelowQuorum.as_str(), "below_quorum");
    }
}
