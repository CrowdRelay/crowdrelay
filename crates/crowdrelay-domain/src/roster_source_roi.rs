//! Roster-level source ROI (5.4, N.6) — the same channel measurement, pooled
//! across every act in one organisation.
//!
//! # Why pooling is the whole feature
//!
//! §4e-4 already ranks acquisition channels for a single act, net of churn, and
//! on a roster it answers "insufficient evidence" over and over. It is right to:
//! eight acts with forty signups each are eight samples of forty, and a sample
//! of forty cannot separate a channel where one person in five stays from one
//! where one in three does. Pooled, it is one sample of three hundred and
//! twenty, which can. Nothing else about the measurement changes — the same
//! fans, the same definition of staying, the same refusal to call an
//! unattributed signup "direct traffic".
//!
//! # What pooling is allowed to claim
//!
//! Three rules, because one pooled number that overstates is worse than eight
//! honest refusals:
//!
//! 1. **A rate needs a denominator.** Under [`POOLED_EVIDENCE_FLOOR`] signups a
//!    channel keeps its counts and gets no verdict, and the read says how many
//!    more arrivals it needs. The rate is never rounded away and never reported
//!    as zero.
//! 2. **One act is not a roster.** A channel whose every signup came from a
//!    single act is marked [`Evidence::SingleAct`]. That is one act's number
//!    wearing the roster's label, and it is not evidence that the channel
//!    travels to the next act — which is the only question a roster-level
//!    ranking is asked.
//! 3. **A difference nobody can measure is not a difference.** Two rates are
//!    only called apart when the gap between them exceeds both of their 95%
//!    half-widths added together. That test is deliberately stricter than a
//!    two-proportion test: it refuses to claim a winner in exactly the band
//!    where a roster would otherwise move its effort on noise.
//!
//! The output is one of four findings, and three of them are refusals to
//! reallocate. That ratio is the point — this read exists to change a decision
//! when the evidence supports changing it, not to produce a leaderboard.

use serde::{Deserialize, Serialize};

/// Signups a pooled channel needs before its rate is allowed a verdict.
///
/// Thirty is where the normal approximation this module's separation test uses
/// stops embarrassing itself, and it is also roughly where the 95% half-width
/// of a one-in-four rate drops under sixteen points — still wide, but no longer
/// wide enough to contain every plausible channel at once.
pub const POOLED_EVIDENCE_FLOOR: u32 = 30;

/// 95% two-sided normal quantile. Named because a bare 1.96 in the arithmetic
/// is the kind of constant a later reader changes without knowing what it buys.
const CONFIDENCE_Z: f64 = 1.96;

/// One channel's pooled counts, as the adapter measured them.
///
/// `acts` is the number of *distinct* acts that produced at least one signup
/// through the channel, counted in SQL rather than summed here — summing
/// per-row act counts would double an act that appears under two creatives.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelSample {
    pub source: String,
    pub community: Option<String>,
    pub acts: u16,
    pub signups: u32,
    /// Mature signups who are still active fans, still consented, and did
    /// something meaningful after their 30-day anniversary and inside the
    /// current 30-day window. Churn stays in `signups` and therefore lowers
    /// this count's rate; an unsubscribe cannot erase a failed acquisition.
    pub stayed_30d: u32,
}

/// How much a channel's rate is allowed to be leaned on.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "evidence")]
pub enum Evidence {
    /// Over the floor, and more than one act contributed. The only shape that
    /// can win a reallocation.
    Pooled,
    /// Over the floor, but every signup came from one act. Reported, ranked,
    /// and never used as proof that the channel works for the roster.
    SingleAct,
    /// Under the floor. Carries what is missing rather than a verdict.
    Insufficient { signups_short_by: u32 },
}

/// One channel as the roster should read it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RankedChannel {
    pub source: String,
    pub community: Option<String>,
    pub acts: u16,
    pub signups: u32,
    pub stayed_30d: u32,
    /// `None` when nobody arrived through the channel: a rate over an empty
    /// denominator is absent, not zero.
    pub stayed_basis_points: Option<u32>,
    pub evidence: Evidence,
}

impl RankedChannel {
    /// `reddit/r-metal`, or `reddit` when the link named no community. The
    /// operator-facing name of the channel, and the key two findings compare.
    #[must_use]
    pub fn label(&self) -> String {
        match &self.community {
            Some(community) => format!("{}/{}", self.source, community),
            None => self.source.clone(),
        }
    }

    const fn clears_floor(&self) -> bool {
        !matches!(self.evidence, Evidence::Insufficient { .. })
    }
}

/// What the roster should do about the ranking, in the operator's words.
///
/// Every arm carries the sentence rather than leaving the caller to compose
/// one: the console, the briefing and any later digest must say the same thing
/// about the same numbers, and three renderers of one finding is three chances
/// to disagree about what it meant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "finding")]
pub enum Finding {
    /// No channel clears the floor even pooled. The roster's answer is the same
    /// one each act got, and saying so is the honest outcome — not a ranking of
    /// noise.
    InsufficientEvidence { message: String },
    /// The channel carrying the most arrivals is also the one people stay
    /// through. Nothing to move.
    EffortIsWellPlaced { channel: String, message: String },
    /// A different channel keeps people better than the busy one, by a margin
    /// wider than both intervals. This is the finding the item was built for.
    Reallocate {
        from: String,
        to: String,
        message: String,
    },
    /// The leader is ahead on the point estimate and the intervals still
    /// overlap. Named rather than hidden, because "keep going until the
    /// sample is bigger" is an action too.
    TooCloseToCall {
        leader: String,
        busiest: String,
        message: String,
    },
}

/// What the adapter counted, before any of it is ranked.
///
/// One struct rather than three arguments, because the three travel together
/// everywhere and a call site that swaps two `u32`s would be silently wrong.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PooledCounts {
    pub samples: Vec<ChannelSample>,
    /// Acts in the organisation with at least one active fan. This is roster
    /// coverage context, not the retention-rate denominator — channel rates use
    /// matured acquisitions, including people who later left.
    pub acts_with_fans: u16,
    /// Arrivals that could not be traced to any channel. Carried into the read
    /// because a leaderboard over a tenth of the roster's arrivals is a
    /// leaderboard of the tenth that happened to be tracked, and the operator
    /// has to be able to see that.
    pub unattributed_signups: u32,
}

/// The roster's channel read.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RosterSourceRoi {
    pub channels: Vec<RankedChannel>,
    /// Acts in the organisation with at least one active fan.
    pub acts_with_fans: u16,
    pub pooled_signups: u32,
    pub pooled_stayed_30d: u32,
    /// Arrivals with no traceable channel. Not a channel and never ranked.
    pub unattributed_signups: u32,
    pub evidence_floor: u32,
    pub finding: Finding,
}

/// Ranks the pooled channels and says what, if anything, follows from them.
///
/// Ordering is: channels that clear the floor first, then by retention rate
/// descending, then by signups descending, then by label — a total order, so
/// two reads of unchanged data cannot disagree about which channel is second.
#[must_use]
pub fn rank_pooled_channels(counts: &PooledCounts) -> RosterSourceRoi {
    let mut channels: Vec<RankedChannel> = counts.samples.iter().map(rank_one).collect();
    channels.sort_by(|left, right| {
        right
            .clears_floor()
            .cmp(&left.clears_floor())
            .then(
                right
                    .stayed_basis_points
                    .unwrap_or(0)
                    .cmp(&left.stayed_basis_points.unwrap_or(0)),
            )
            .then(right.signups.cmp(&left.signups))
            .then(left.label().cmp(&right.label()))
    });

    let pooled_signups = channels
        .iter()
        .fold(0u32, |total, channel| total.saturating_add(channel.signups));
    let pooled_stayed = channels.iter().fold(0u32, |total, channel| {
        total.saturating_add(channel.stayed_30d)
    });
    let finding = decide(&channels);

    RosterSourceRoi {
        channels,
        acts_with_fans: counts.acts_with_fans,
        pooled_signups,
        pooled_stayed_30d: pooled_stayed,
        unattributed_signups: counts.unattributed_signups,
        evidence_floor: POOLED_EVIDENCE_FLOOR,
        finding,
    }
}

fn rank_one(sample: &ChannelSample) -> RankedChannel {
    let stayed_basis_points = (sample.signups > 0).then(|| {
        u32::try_from(
            u64::from(sample.stayed_30d).saturating_mul(10_000) / u64::from(sample.signups),
        )
        .unwrap_or(u32::MAX)
    });
    let evidence = if sample.signups < POOLED_EVIDENCE_FLOOR {
        Evidence::Insufficient {
            signups_short_by: POOLED_EVIDENCE_FLOOR - sample.signups,
        }
    } else if sample.acts <= 1 {
        Evidence::SingleAct
    } else {
        Evidence::Pooled
    };
    RankedChannel {
        source: sample.source.clone(),
        community: sample.community.clone(),
        acts: sample.acts,
        signups: sample.signups,
        stayed_30d: sample.stayed_30d,
        stayed_basis_points,
        evidence,
    }
}

/// The finding, from the ranked list.
///
/// The leader must be [`Evidence::Pooled`]: a single act's channel may top the
/// table and is shown doing so, but it cannot be the reason a roster moves
/// effort, because the question is whether the channel travels between acts and
/// one act's numbers do not answer it.
fn decide(channels: &[RankedChannel]) -> Finding {
    let leader = channels
        .iter()
        .filter(|channel| matches!(channel.evidence, Evidence::Pooled))
        .max_by_key(|channel| (channel.stayed_basis_points.unwrap_or(0), channel.signups));
    let Some(leader) = leader else {
        return Finding::InsufficientEvidence {
            message: format!(
                "No channel has {POOLED_EVIDENCE_FLOOR} attributed arrivals from more than one \
                 act yet, so the roster cannot rank its channels any better than each act can \
                 alone. The counts below are real; the ranking they would produce is not."
            ),
        };
    };

    // Where the effort currently goes: the most arrivals, whatever their
    // quality. Comparing the leader against the total, or against the second
    // best, would answer a question nobody asked.
    let busiest = channels
        .iter()
        .filter(|channel| channel.clears_floor())
        .max_by_key(|channel| (channel.signups, channel.stayed_basis_points.unwrap_or(0)));
    let Some(busiest) = busiest else {
        return Finding::InsufficientEvidence {
            message: "Nothing clears the evidence floor.".to_owned(),
        };
    };

    if busiest.label() == leader.label() {
        return Finding::EffortIsWellPlaced {
            channel: leader.label(),
            message: format!(
                "{} brings the most people and keeps the most of them — {} of every 100 arrivals \
                 are still here after 30 days, across {} acts. The roster's effort is already on \
                 the channel the evidence prefers.",
                leader.label(),
                percent(leader.stayed_basis_points),
                leader.acts
            ),
        };
    }

    if separated(leader, busiest) {
        return Finding::Reallocate {
            from: busiest.label(),
            to: leader.label(),
            message: format!(
                "{} carries most of the roster's arrivals ({} signups, {} of every 100 still here \
                 after 30 days), but {} keeps people better — {} of every 100, over {} signups \
                 from {} acts. The gap is wider than both samples' margins, so it is a real \
                 difference and not this month's noise.",
                busiest.label(),
                busiest.signups,
                percent(busiest.stayed_basis_points),
                leader.label(),
                percent(leader.stayed_basis_points),
                leader.signups,
                leader.acts
            ),
        };
    }

    Finding::TooCloseToCall {
        leader: leader.label(),
        busiest: busiest.label(),
        message: format!(
            "{} is ahead of {} on retention ({} against {} of every 100), but the two samples' \
             margins still overlap. Moving the roster's effort on this gap would be moving it on \
             noise; the ranking is worth re-reading once either channel has more arrivals.",
            leader.label(),
            busiest.label(),
            percent(leader.stayed_basis_points),
            percent(busiest.stayed_basis_points)
        ),
    }
}

/// Whether `better`'s rate is far enough above `worse`'s to act on.
///
/// The test: the gap must exceed the sum of both 95% half-widths. That is
/// stricter than a two-proportion z-test — it is the "do the error bars touch"
/// rule — and it is chosen on purpose. The cost of claiming a difference that
/// is not there is a roster moving its whole promotion budget onto a channel
/// that was lucky; the cost of missing one is waiting another month for more
/// arrivals, which happen anyway.
fn separated(better: &RankedChannel, worse: &RankedChannel) -> bool {
    let (Some(better_bp), Some(worse_bp)) = (better.stayed_basis_points, worse.stayed_basis_points)
    else {
        return false;
    };
    if better_bp <= worse_bp || better.signups == 0 || worse.signups == 0 {
        return false;
    }
    let better_rate = f64::from(better_bp) / 10_000.0;
    let worse_rate = f64::from(worse_bp) / 10_000.0;
    better_rate - worse_rate
        > half_width(better_rate, better.signups) + half_width(worse_rate, worse.signups)
}

/// The 95% half-width of a proportion, with the degenerate ends held away from
/// the boundary.
///
/// At p = 0 or p = 1 the normal interval has zero width, which would let "0 of
/// 30 stayed" and "30 of 30 stayed" be declared different from anything at all.
/// Clamping to half an observation inside the range is the standard patch and
/// keeps the interval honest where the approximation is worst.
fn half_width(rate: f64, signups: u32) -> f64 {
    let n = f64::from(signups);
    let guard = 0.5 / n;
    let rate = rate.clamp(guard, 1.0 - guard);
    CONFIDENCE_Z * (rate * (1.0 - rate) / n).sqrt()
}

/// Basis points as whole percent, for a sentence. `None` becomes `—` rather
/// than `0`: a rate nobody could compute is not a rate of nothing.
fn percent(basis_points: Option<u32>) -> String {
    basis_points.map_or_else(|| "—".to_owned(), |bp| (bp / 100).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(samples: Vec<ChannelSample>, acts_with_fans: u16) -> PooledCounts {
        PooledCounts {
            samples,
            acts_with_fans,
            unattributed_signups: 0,
        }
    }

    fn sample(source: &str, acts: u16, signups: u32, stayed: u32) -> ChannelSample {
        ChannelSample {
            source: source.to_owned(),
            community: None,
            acts,
            signups,
            stayed_30d: stayed,
        }
    }

    /// The n-problem dissolving is the item's whole argument, so the case worth
    /// asserting first is the one where each act alone would refuse: four acts
    /// of twelve signups is one channel of forty-eight, and forty-eight clears
    /// the floor.
    #[test]
    fn four_small_acts_make_one_usable_sample() {
        let read = rank_pooled_channels(&counts(vec![sample("reddit", 4, 48, 18)], 4));
        assert_eq!(read.channels[0].evidence, Evidence::Pooled);
        assert_eq!(read.channels[0].stayed_basis_points, Some(3750));
        assert_eq!(read.pooled_signups, 48);
    }

    /// Rule 2. The count is real and shown; the verdict is not, because the
    /// question a roster asks is whether the channel travels to the next act.
    #[test]
    fn one_acts_channel_is_never_pooled_evidence() {
        let read = rank_pooled_channels(&counts(vec![sample("discord", 1, 400, 200)], 3));
        assert_eq!(read.channels[0].evidence, Evidence::SingleAct);
        assert_eq!(read.channels[0].signups, 400);
        assert!(
            matches!(read.finding, Finding::InsufficientEvidence { .. }),
            "a single act's channel led the table and was allowed to be the finding: {:?}",
            read.finding
        );
    }

    /// Rule 1. Under the floor there is no verdict, and the shortfall is named
    /// so the operator knows what would end the refusal.
    #[test]
    fn a_thin_channel_says_what_it_is_missing() {
        let read = rank_pooled_channels(&counts(vec![sample("facebook", 3, 11, 9)], 3));
        assert_eq!(
            read.channels[0].evidence,
            Evidence::Insufficient {
                signups_short_by: 19
            }
        );
        // The rate is still computed and still shown — 9 of 11 is a fact. What
        // is withheld is the ranking's endorsement of it.
        assert_eq!(read.channels[0].stayed_basis_points, Some(8181));
    }

    /// Absent is not zero. A channel with a link, clicks and no signups has no
    /// retention rate at all.
    #[test]
    fn no_arrivals_is_no_rate() {
        let read = rank_pooled_channels(&counts(vec![sample("venue", 2, 0, 0)], 2));
        assert_eq!(read.channels[0].stayed_basis_points, None);
    }

    /// The finding this whole item exists to produce.
    #[test]
    fn a_separated_gap_moves_the_roster() {
        let read = rank_pooled_channels(&counts(
            vec![sample("facebook", 5, 600, 60), sample("reddit", 4, 200, 80)],
            5,
        ));
        match &read.finding {
            Finding::Reallocate { from, to, message } => {
                assert_eq!(from, "facebook");
                assert_eq!(to, "reddit");
                assert!(
                    message.contains("600"),
                    "the busy channel's size is part of the case"
                );
            }
            other => panic!("expected a reallocation, got {other:?}"),
        }
    }

    /// Rule 3. Same direction, sample small enough that the intervals touch —
    /// and the read says so instead of ranking anyway.
    #[test]
    fn an_overlapping_gap_moves_nothing() {
        let read = rank_pooled_channels(&counts(
            vec![sample("facebook", 4, 40, 12), sample("reddit", 3, 32, 12)],
            4,
        ));
        assert!(
            matches!(read.finding, Finding::TooCloseToCall { .. }),
            "a 30%-against-37.5% gap on 40 and 32 signups was called: {:?}",
            read.finding
        );
    }

    /// The degenerate end of the normal approximation: a perfect rate must not
    /// get a zero-width interval and win by default.
    #[test]
    fn a_perfect_small_sample_does_not_win_by_having_no_width() {
        let read = rank_pooled_channels(&counts(
            vec![sample("facebook", 4, 300, 90), sample("reddit", 2, 30, 30)],
            4,
        ));
        assert!(
            matches!(
                read.finding,
                Finding::TooCloseToCall { .. } | Finding::Reallocate { .. }
            ),
            "unexpected finding: {:?}",
            read.finding
        );
        // 30 of 30 against 90 of 300 is a real gap and may be called; what must
        // never happen is the interval being zero-width, which would let
        // 30-of-30 beat a channel it is statistically tied with.
        assert!(half_width(1.0, 30) > 0.0);
    }

    /// Nothing to move when the busy channel is also the good one — and the
    /// message has to say that, because silence reads as "no data".
    #[test]
    fn the_busy_channel_can_also_be_the_right_one() {
        let read = rank_pooled_channels(&counts(
            vec![sample("reddit", 4, 500, 200), sample("facebook", 3, 60, 6)],
            4,
        ));
        match &read.finding {
            Finding::EffortIsWellPlaced { channel, .. } => assert_eq!(channel, "reddit"),
            other => panic!("expected effort to be well placed, got {other:?}"),
        }
    }

    /// Ordering is total: the floor first, then the rate, then the size, then
    /// the name. Two reads of unchanged data must not disagree about second
    /// place.
    #[test]
    fn the_ranking_is_a_total_order() {
        let read = rank_pooled_channels(&counts(
            vec![
                sample("thin", 2, 10, 10),
                sample("beta", 2, 100, 40),
                sample("alpha", 2, 100, 40),
            ],
            2,
        ));
        let labels: Vec<String> = read.channels.iter().map(RankedChannel::label).collect();
        assert_eq!(labels, vec!["alpha", "beta", "thin"]);
    }

    /// The community is part of the channel's identity: r/metal converting and
    /// r/indie not converting is the answer, and "reddit" is not.
    #[test]
    fn a_community_is_named_in_the_label() {
        let read = rank_pooled_channels(&counts(
            vec![ChannelSample {
                source: "reddit".to_owned(),
                community: Some("r-metal".to_owned()),
                acts: 2,
                signups: 50,
                stayed_30d: 20,
            }],
            2,
        ));
        assert_eq!(read.channels[0].label(), "reddit/r-metal");
    }
}
