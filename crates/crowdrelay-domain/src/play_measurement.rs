//! What a play actually did, and what kind of claim that number supports.
//!
//! The unit of measurement is the play. Measuring a single send would attribute
//! a whole campaign to whichever message happened to be last, and measuring the
//! series without a frozen pre-play baseline would compare the play against a
//! number the play already moved.
//!
//! Two claims live here and they are never allowed to merge.
//!
//! * [`PlayClaim::Attributed`] is first-party. Our own rows join the outcome to
//!   the action — a link this play minted, a click we recorded against it.
//! * [`PlayClaim::Correlational`] is a series that moved after the play ran.
//!   Nothing joins the two. It is the weaker claim and it is reported as the
//!   weaker claim, because a tracker count rising in the same fortnight as a
//!   campaign is evidence of a coincidence until something says otherwise.
//!
//! The third answer is the one that makes the other two trustworthy: when the
//! comparison cannot be made, this module returns [`PlayEffect::Insufficient`]
//! with the reason. Nothing here interpolates a missing point, substitutes the
//! other claim's number, or reports a percentage against a baseline too flat to
//! carry one.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{growth_metrics::MetricDirection, performance::EffectAssessment};

/// What a number is allowed to say it proves.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayClaim {
    /// Joined to the play through our own rows. The strong claim, and the only
    /// one that may be called attribution.
    Attributed,
    /// The play's success metric moved over the play's window. No join exists;
    /// the movement and the campaign merely share a period.
    Correlational,
}

impl PlayClaim {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Attributed => "attributed",
            Self::Correlational => "correlational",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "attributed" => Some(Self::Attributed),
            "correlational" => Some(Self::Correlational),
            _ => None,
        }
    }

    /// Both claims, opened together when a play starts.
    ///
    /// The attributed one is opened even when nothing can currently satisfy it.
    /// An absent row is invisible; a row that says the claim cannot be made, and
    /// why, is the entire point of measuring.
    #[must_use]
    pub const fn all() -> [Self; 2] {
        [Self::Correlational, Self::Attributed]
    }

    /// Prose an operator reads next to the number, so the strength of the claim
    /// travels with it instead of living in a schema somebody has to look up.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Attributed => {
                "joined to this play through our own rows, so the play is a cause of the number"
            }
            Self::Correlational => {
                "the play's success metric over the play's window, against the same series before \
                 it. The movement and the campaign share a period; nothing joins them"
            }
        }
    }
}

/// Why a claim cannot be made. Recorded on the row, because an unexplained gap
/// reads as a system that failed rather than as a question nobody can answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InsufficientEvidence {
    /// The series had no usable trend when the play started. Nothing to compare
    /// the window against, and reconstructing one now would read a baseline the
    /// play has already moved.
    NoBaseline,
    /// The series has no usable observation at the end of the window.
    NoObservation,
    /// The window has no length, so a per-day rate cannot be formed.
    WindowNotClosed,
    /// The first-party join key does not exist. A play that mints no tracked
    /// link cannot be credited with a click, and borrowing the correlational
    /// number here would turn a coincidence into a claimed cause.
    NoAttributionKey,
    /// The play never reached anybody, so there is nothing whose effect could
    /// be measured. Distinct from a null result: a campaign that did not run is
    /// not a campaign that did not work.
    NothingDelivered,
    /// More than one live series answers to the play's success metric, so the
    /// play cannot say which one it moved. Adding them together would merge two
    /// artists' audiences into one timeline, which is the interleaving the
    /// series design refuses in the first place.
    AmbiguousSeries,
}

impl InsufficientEvidence {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoBaseline => "no_baseline",
            Self::NoObservation => "no_observation",
            Self::WindowNotClosed => "window_not_closed",
            Self::NoAttributionKey => "no_attribution_key",
            Self::NothingDelivered => "nothing_delivered",
            Self::AmbiguousSeries => "ambiguous_series",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "no_baseline" => Some(Self::NoBaseline),
            "no_observation" => Some(Self::NoObservation),
            "window_not_closed" => Some(Self::WindowNotClosed),
            "no_attribution_key" => Some(Self::NoAttributionKey),
            "nothing_delivered" => Some(Self::NothingDelivered),
            "ambiguous_series" => Some(Self::AmbiguousSeries),
            _ => None,
        }
    }
}

/// The measured effect of one play on one claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "evidence", rename_all = "snake_case")]
pub enum PlayEffect {
    Measured {
        assessment: EffectAssessment,
        /// The window's rate against the pre-play rate. `None` when the
        /// baseline is too flat to carry a percentage — the movement is real,
        /// the ratio would be invented.
        delta_basis_points: Option<i32>,
        /// Both rates, so an operator can check the verdict rather than trust
        /// it. Milli-units per day, oriented so that higher is better.
        baseline_milli_per_day: i64,
        window_milli_per_day: i64,
    },
    Insufficient {
        reason: InsufficientEvidence,
    },
}

impl PlayEffect {
    #[must_use]
    pub const fn assessment(self) -> Option<EffectAssessment> {
        match self {
            Self::Measured { assessment, .. } => Some(assessment),
            Self::Insufficient { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct PlayMeasurementPolicy {
    /// How long after a play's last step closes before its window is read.
    /// A tracker count does not move the hour a message lands.
    pub settle_days: u16,
    /// Movement inside this band of the pre-play rate is called neutral. Wider
    /// than the band used for a single action's effect, because a rate derived
    /// from a fortnight of daily points is a noisier number than a revenue
    /// total over three days.
    pub neutral_band_basis_points: u16,
    /// Absolute movement below this is noise whatever the ratio says. Without
    /// it a series creeping from one new tracker a day to two reports as a
    /// hundred per cent improvement, which is true and useless.
    pub minimum_movement_milli_per_day: i64,
    /// A pre-play rate flatter than this cannot carry a percentage. The verdict
    /// still stands on absolute movement; only the ratio is withheld.
    pub minimum_baseline_milli_per_day: i64,
}

impl Default for PlayMeasurementPolicy {
    fn default() -> Self {
        Self {
            settle_days: 7,
            neutral_band_basis_points: 1_000,
            // A tenth of a unit per day. Below this, a tracker series is
            // standing still whatever the arithmetic says.
            minimum_movement_milli_per_day: 100,
            minimum_baseline_milli_per_day: 500,
        }
    }
}

/// The rate a play's window implies, in milli-units per day.
///
/// `None` for a window shorter than a day: a per-day rate extrapolated from a
/// few hours is a number nobody measured, and the honest answer is that the
/// window has not closed.
///
/// Scaled from seconds rather than whole days on purpose. Truncating to whole
/// days turns a window a fraction under ten days into nine, which inflates
/// every rate derived from it by a tenth — a systematic overstatement that
/// would land squarely on the side of claiming the play worked.
#[must_use]
pub fn window_velocity_milli_per_day(
    baseline_value: i64,
    observed_value: i64,
    window: Duration,
) -> Option<i64> {
    let seconds = window.whole_seconds();
    if seconds < SECONDS_PER_DAY {
        return None;
    }
    let movement = i128::from(observed_value) - i128::from(baseline_value);
    let rate = movement * 1_000 * i128::from(SECONDS_PER_DAY) / i128::from(seconds);
    i64::try_from(rate).ok()
}

const SECONDS_PER_DAY: i64 = 86_400;

/// When a play's window closes, given its last step's expiry.
#[must_use]
pub fn measurement_due_at(
    last_step_expires_at: OffsetDateTime,
    policy: PlayMeasurementPolicy,
) -> OffsetDateTime {
    last_step_expires_at + Duration::days(i64::from(policy.settle_days))
}

/// Classifies one play's window against its own pre-play rate.
///
/// Direction is applied to both rates before anything is compared, so a series
/// where falling is good is judged on its own terms rather than by flipping the
/// answer afterwards.
#[must_use]
pub fn assess_play_effect(
    baseline_milli_per_day: Option<i64>,
    window_milli_per_day: Option<i64>,
    direction: MetricDirection,
    policy: PlayMeasurementPolicy,
) -> PlayEffect {
    let Some(baseline) = baseline_milli_per_day.map(|rate| direction.orient(rate)) else {
        return PlayEffect::Insufficient {
            reason: InsufficientEvidence::NoBaseline,
        };
    };
    let Some(window) = window_milli_per_day.map(|rate| direction.orient(rate)) else {
        return PlayEffect::Insufficient {
            reason: InsufficientEvidence::NoObservation,
        };
    };

    let movement = window.saturating_sub(baseline);
    // The absolute floor is checked first and on its own. A ratio can make a
    // trivial movement look decisive, and no percentage should be able to
    // overrule the fact that almost nothing happened.
    if movement.abs() < policy.minimum_movement_milli_per_day.max(1) {
        return PlayEffect::Measured {
            assessment: EffectAssessment::Neutral,
            delta_basis_points: relative_delta(baseline, movement, policy),
            baseline_milli_per_day: baseline,
            window_milli_per_day: window,
        };
    }

    let delta_basis_points = relative_delta(baseline, movement, policy);
    let assessment = match delta_basis_points {
        // With a baseline worth dividing by, the neutral band is relative: a
        // ten per cent change on a rate is not a result.
        Some(delta) => {
            let band = i32::from(policy.neutral_band_basis_points);
            if delta > band {
                EffectAssessment::Improved
            } else if delta < -band {
                EffectAssessment::Worsened
            } else {
                EffectAssessment::Neutral
            }
        }
        // Against a flat baseline the movement itself is the whole finding,
        // and it already cleared the absolute floor above.
        None if movement > 0 => EffectAssessment::Improved,
        None => EffectAssessment::Worsened,
    };
    PlayEffect::Measured {
        assessment,
        delta_basis_points,
        baseline_milli_per_day: baseline,
        window_milli_per_day: window,
    }
}

/// Everything one claim needs to be settled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlayOutcomeInput {
    pub claim: PlayClaim,
    /// Fans the play actually reached. Zero is not a null result.
    pub recipients_reached: u32,
    pub baseline_milli_per_day: Option<i64>,
    pub window_milli_per_day: Option<i64>,
    /// Clicks our own rows join to this play. `None` means no join key exists,
    /// which is a different fact from zero clicks.
    pub attributed_clicks: Option<i64>,
    pub direction: MetricDirection,
    /// True when more than one live series answers to the play's success
    /// metric. Carried rather than resolved by the reader, because the obvious
    /// resolutions — pick the first, add them up — are both wrong.
    pub ambiguous_series: bool,
}

/// How one claim settles.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "evidence", rename_all = "snake_case")]
pub enum PlayOutcomeVerdict {
    Measured {
        /// `None` when the claim is a count with nothing to compare it to. A
        /// first-party click total is a measured fact and not a verdict, and
        /// dressing it as `improved` would invent the comparison.
        assessment: Option<EffectAssessment>,
        delta_basis_points: Option<i32>,
    },
    Insufficient {
        reason: InsufficientEvidence,
    },
}

/// Settles one claim about one play.
///
/// The order is the rule. Reach is checked before anything else, because a
/// campaign that reached nobody has no effect to measure and reporting it as
/// `neutral` would put a null result where a non-event belongs. Then the claims
/// diverge and never meet again: the attributed one may only speak from a join
/// key, and the correlational one may only speak from the series' own rates.
#[must_use]
pub fn assess_play_outcome(
    input: PlayOutcomeInput,
    policy: PlayMeasurementPolicy,
) -> PlayOutcomeVerdict {
    if input.recipients_reached == 0 {
        return PlayOutcomeVerdict::Insufficient {
            reason: InsufficientEvidence::NothingDelivered,
        };
    }
    match input.claim {
        PlayClaim::Attributed => match input.attributed_clicks {
            // A count, not a comparison. There is no pre-play click total for a
            // link that did not exist before the play, so no verdict is offered.
            Some(_) => PlayOutcomeVerdict::Measured {
                assessment: None,
                delta_basis_points: None,
            },
            None => PlayOutcomeVerdict::Insufficient {
                reason: InsufficientEvidence::NoAttributionKey,
            },
        },
        PlayClaim::Correlational if input.ambiguous_series => PlayOutcomeVerdict::Insufficient {
            reason: InsufficientEvidence::AmbiguousSeries,
        },
        PlayClaim::Correlational => match assess_play_effect(
            input.baseline_milli_per_day,
            input.window_milli_per_day,
            input.direction,
            policy,
        ) {
            PlayEffect::Measured {
                assessment,
                delta_basis_points,
                ..
            } => PlayOutcomeVerdict::Measured {
                assessment: Some(assessment),
                delta_basis_points,
            },
            PlayEffect::Insufficient { reason } => PlayOutcomeVerdict::Insufficient { reason },
        },
    }
}

/// The movement as a share of the pre-play rate, or `None` when that rate is
/// too flat to divide by.
fn relative_delta(baseline: i64, movement: i64, policy: PlayMeasurementPolicy) -> Option<i32> {
    let denominator = baseline.abs();
    if denominator < policy.minimum_baseline_milli_per_day.max(1) {
        return None;
    }
    let raw = movement.saturating_mul(10_000) / denominator;
    Some(i32::try_from(raw).unwrap_or(if raw > 0 { i32::MAX } else { i32::MIN }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> PlayMeasurementPolicy {
        PlayMeasurementPolicy::default()
    }

    #[test]
    fn a_faster_window_than_the_pre_play_rate_is_an_improvement() {
        let effect = assess_play_effect(
            Some(1_000),
            Some(2_000),
            MetricDirection::HigherIsBetter,
            policy(),
        );
        assert_eq!(
            effect,
            PlayEffect::Measured {
                assessment: EffectAssessment::Improved,
                delta_basis_points: Some(10_000),
                baseline_milli_per_day: 1_000,
                window_milli_per_day: 2_000,
            }
        );
    }

    #[test]
    fn a_small_movement_stays_neutral_however_large_the_ratio() {
        // One extra tracker every twenty days against a baseline of one every
        // twenty. Doubling, and nothing happened.
        let effect = assess_play_effect(
            Some(50),
            Some(100),
            MetricDirection::HigherIsBetter,
            policy(),
        );
        assert_eq!(effect.assessment(), Some(EffectAssessment::Neutral));
    }

    #[test]
    fn a_flat_baseline_yields_a_verdict_but_never_an_invented_percentage() {
        let effect = assess_play_effect(
            Some(0),
            Some(4_000),
            MetricDirection::HigherIsBetter,
            policy(),
        );
        assert_eq!(effect.assessment(), Some(EffectAssessment::Improved));
        assert!(
            matches!(
                effect,
                PlayEffect::Measured {
                    delta_basis_points: None,
                    ..
                }
            ),
            "a percentage against a standing-still series is a number nobody measured"
        );
    }

    #[test]
    fn a_missing_baseline_is_refused_rather_than_reconstructed() {
        assert_eq!(
            assess_play_effect(None, Some(4_000), MetricDirection::HigherIsBetter, policy()),
            PlayEffect::Insufficient {
                reason: InsufficientEvidence::NoBaseline
            }
        );
        assert_eq!(
            assess_play_effect(Some(1_000), None, MetricDirection::HigherIsBetter, policy()),
            PlayEffect::Insufficient {
                reason: InsufficientEvidence::NoObservation
            }
        );
    }

    #[test]
    fn an_insufficient_outcome_carries_no_verdict() {
        let effect = assess_play_effect(None, None, MetricDirection::HigherIsBetter, policy());
        assert_eq!(effect.assessment(), None);
    }

    #[test]
    fn a_series_where_falling_is_good_is_judged_on_its_own_terms() {
        // Unsubscribes: the pre-play rate was five a day, the window was one.
        let effect = assess_play_effect(
            Some(5_000),
            Some(1_000),
            MetricDirection::LowerIsBetter,
            policy(),
        );
        assert_eq!(effect.assessment(), Some(EffectAssessment::Improved));
    }

    #[test]
    fn a_slowing_series_is_a_worsening_even_while_it_still_grows() {
        let effect = assess_play_effect(
            Some(4_000),
            Some(1_000),
            MetricDirection::HigherIsBetter,
            policy(),
        );
        assert_eq!(effect.assessment(), Some(EffectAssessment::Worsened));
    }

    #[test]
    fn a_window_shorter_than_a_day_produces_no_rate() {
        assert_eq!(
            window_velocity_milli_per_day(10, 20, Duration::hours(4)),
            None
        );
        assert_eq!(
            window_velocity_milli_per_day(10, 20, Duration::days(10)),
            Some(1_000)
        );
    }

    #[test]
    fn a_window_a_fraction_short_of_whole_days_is_not_rounded_up() {
        // Truncating to whole days would read this as nine, inflating the rate
        // by a tenth — always in the direction of claiming the play worked.
        let almost_ten = Duration::days(10) - Duration::seconds(1);
        let rate = window_velocity_milli_per_day(0, 100, almost_ten)
            .expect("a window over a day has a rate");
        assert_eq!(rate, 10_000, "one hundred over ten days, not over nine");
    }

    #[test]
    fn both_claims_are_opened_and_they_are_not_the_same_claim() {
        assert_eq!(PlayClaim::all().len(), 2);
        assert_ne!(
            PlayClaim::Attributed.description(),
            PlayClaim::Correlational.description()
        );
        for claim in PlayClaim::all() {
            assert_eq!(PlayClaim::parse(claim.as_str()), Some(claim));
        }
    }

    fn outcome(claim: PlayClaim) -> PlayOutcomeInput {
        PlayOutcomeInput {
            claim,
            recipients_reached: 40,
            baseline_milli_per_day: Some(1_000),
            window_milli_per_day: Some(2_000),
            attributed_clicks: None,
            direction: MetricDirection::HigherIsBetter,
            ambiguous_series: false,
        }
    }

    #[test]
    fn two_series_answering_to_one_metric_are_refused_not_added() {
        let input = PlayOutcomeInput {
            ambiguous_series: true,
            ..outcome(PlayClaim::Correlational)
        };
        assert_eq!(
            assess_play_outcome(input, policy()),
            PlayOutcomeVerdict::Insufficient {
                reason: InsufficientEvidence::AmbiguousSeries
            }
        );
    }

    #[test]
    fn a_campaign_that_reached_nobody_is_a_non_event_not_a_null_result() {
        for claim in PlayClaim::all() {
            let input = PlayOutcomeInput {
                recipients_reached: 0,
                attributed_clicks: Some(9),
                ..outcome(claim)
            };
            assert_eq!(
                assess_play_outcome(input, policy()),
                PlayOutcomeVerdict::Insufficient {
                    reason: InsufficientEvidence::NothingDelivered
                },
                "a play that did not run must not report an effect"
            );
        }
    }

    #[test]
    fn the_attributed_claim_never_borrows_the_correlational_number() {
        // The series moved a great deal. With no join key, the attributed claim
        // still has nothing to say, and saying the series' number here would
        // turn a coincidence into a claimed cause.
        let input = PlayOutcomeInput {
            window_milli_per_day: Some(50_000),
            ..outcome(PlayClaim::Attributed)
        };
        assert_eq!(
            assess_play_outcome(input, policy()),
            PlayOutcomeVerdict::Insufficient {
                reason: InsufficientEvidence::NoAttributionKey
            }
        );
    }

    #[test]
    fn an_attributed_count_is_reported_without_a_verdict() {
        let input = PlayOutcomeInput {
            attributed_clicks: Some(12),
            ..outcome(PlayClaim::Attributed)
        };
        assert_eq!(
            assess_play_outcome(input, policy()),
            PlayOutcomeVerdict::Measured {
                assessment: None,
                delta_basis_points: None,
            },
            "there is no pre-play click total, so there is no comparison to report"
        );
    }

    #[test]
    fn zero_attributed_clicks_is_a_measurement_and_a_missing_key_is_not() {
        let measured = PlayOutcomeInput {
            attributed_clicks: Some(0),
            ..outcome(PlayClaim::Attributed)
        };
        assert!(matches!(
            assess_play_outcome(measured, policy()),
            PlayOutcomeVerdict::Measured { .. }
        ));
        assert!(matches!(
            assess_play_outcome(outcome(PlayClaim::Attributed), policy()),
            PlayOutcomeVerdict::Insufficient { .. }
        ));
    }

    #[test]
    fn the_correlational_claim_carries_the_series_verdict() {
        assert_eq!(
            assess_play_outcome(outcome(PlayClaim::Correlational), policy()),
            PlayOutcomeVerdict::Measured {
                assessment: Some(EffectAssessment::Improved),
                delta_basis_points: Some(10_000),
            }
        );
    }

    #[test]
    fn the_window_closes_after_the_last_step_not_with_it() {
        // A tracker count does not move the hour a message lands, and reading
        // it at the last step's expiry would measure a campaign mid-flight.
        let expiry = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        assert_eq!(
            measurement_due_at(expiry, policy()),
            expiry + Duration::days(7)
        );
    }
}
