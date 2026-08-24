//! Talking terms on a live opportunity, once one is actually on the table.
//!
//! Phase 7 computes what a show costs and Phase 8a and 8b decide whether it is
//! worth pursuing at all. This is the part after that: a promoter has named a
//! fee, and somebody has to work out whether it clears, what to ask for
//! instead, and when to stop asking.
//!
//! Two things make this safe to leave running at any autonomy level.
//!
//! 1. **The refusals live here, not in a settings row.** Accepting below the
//!    floor, accepting a contract or an exclusivity clause, accepting into a
//!    year that is already past its stretch, accepting a show whose cost could
//!    not be computed — none of these are switches an operator can loosen by
//!    accident, because they are not switches.
//! 2. **The arithmetic and the drafting are the slow parts, and they are what
//!    the agent does.** At the current posture every counter and every
//!    acceptance is `third_party` and therefore approval-gated. The agent works
//!    out the ladder, drafts the move and parks it. That is most of the value
//!    already, and widening it later changes one ceiling row rather than
//!    anything in this file.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{
    TeamOpportunityId,
    live_opportunities::{LiveOpportunityPolicy, LiveOpportunitySnapshot, StrategicTier},
};

/// Where a negotiation has got to.
///
/// `Countered` is not a pause: it means the agent has asked for something and
/// the promoter has not answered yet. A promoter who improves their offer puts
/// the negotiation back to `Proposed` with a better number, which is what makes
/// a second round a round rather than a new conversation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TermsState {
    Proposed,
    Countered,
    Accepted,
    Declined,
    Expired,
}

impl TermsState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Countered => "countered",
            Self::Accepted => "accepted",
            Self::Declined => "declined",
            Self::Expired => "expired",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "proposed" => Some(Self::Proposed),
            "countered" => Some(Self::Countered),
            "accepted" => Some(Self::Accepted),
            "declined" => Some(Self::Declined),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }

    /// True once the negotiation is over. A terminal state is never revisited,
    /// so a promoter who comes back later is a new conversation an operator
    /// opens deliberately rather than a state machine quietly reopening.
    #[must_use]
    pub const fn settled(self) -> bool {
        matches!(self, Self::Accepted | Self::Declined | Self::Expired)
    }
}

/// The three numbers a negotiation is conducted against.
///
/// Frozen when the negotiation opens. A ladder that moved under a running
/// conversation would make the counter the agent sent last week unexplainable
/// from the row today — but the acceptance rule still re-reads the *current*
/// cost, so a ladder frozen against numbers that later turn out to be wrong
/// cannot talk the agent into a show that no longer clears.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct TermsLadder {
    /// Below this the band is working for nothing: cost, plus the minimum
    /// margin, plus whatever the application itself costs. At the Landmark tier
    /// this drops by the operator's bounded loss tolerance and by nothing else.
    pub walk_away_minor: i64,
    /// The fee that makes the show clearly worth playing rather than merely not
    /// a loss.
    pub target_minor: i64,
    /// What to ask for first. Above the target, because an opening ask that is
    /// already the target leaves the band negotiating down from the number they
    /// actually wanted.
    pub opening_ask_minor: i64,
}

/// Why the agent will not accept, whatever the fee says.
///
/// Each of these holds at every autonomy level. They are refusals, not
/// thresholds, which is why they are an enum here rather than columns
/// somewhere.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TermsRefusal {
    /// The offer, after everything the show costs, leaves the band short.
    BelowFloor,
    /// A contract is a legal commitment and is a human's to sign.
    RequiresContract,
    /// Exclusivity binds dates nobody has looked at yet.
    Exclusive,
    /// No date, or a date that has already gone.
    DateNotFree,
    /// The year is already past the stretch the operator set. A Landmark
    /// opportunity is escalated rather than accepted, which is Phase 8b's rule
    /// and is not loosened here.
    PastAnnualStretch,
    /// Inside the stretch but below the bar the operator set for a stretch
    /// show.
    StretchScoreTooLow,
    /// The trip could not be costed from real inputs. An unknown cost is never
    /// a cleared floor.
    CostInsufficient,
}

impl TermsRefusal {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BelowFloor => "below_floor",
            Self::RequiresContract => "requires_contract",
            Self::Exclusive => "exclusive",
            Self::DateNotFree => "date_not_free",
            Self::PastAnnualStretch => "past_annual_stretch",
            Self::StretchScoreTooLow => "stretch_score_too_low",
            Self::CostInsufficient => "cost_insufficient",
        }
    }
}

/// One negotiation as the database holds it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct TermsSnapshot {
    pub opportunity_id: TeamOpportunityId,
    pub state: TermsState,
    /// What the promoter has on the table right now.
    pub offered_fee_minor: i64,
    pub ladder: TermsLadder,
    /// What the agent asked for last, if it has asked.
    pub countered_fee_minor: Option<i64>,
    /// Counters already made. Bounded, because a third ask to somebody who has
    /// improved their offer twice is the band talking itself out of a show.
    pub counter_rounds: u8,
    /// When the promoter's side of this goes cold.
    pub responds_by: OffsetDateTime,
}

/// What the agent wants to do about the terms on the table.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum TermsDecision {
    /// Ask for this instead. Always approval-gated at the current posture.
    Counter { ask_minor: i64, round: u8 },
    /// Take it. Approval-gated at the current posture, and refused outright by
    /// [`TermsRefusal`] whatever the posture.
    Accept { fee_minor: i64 },
    /// Say no, for a stated reason.
    Decline { reason: TermsRefusal },
    /// The promoter's window closed with nothing agreed.
    Expire,
    /// Nothing to do: the agent has asked and is waiting, or the negotiation is
    /// already settled.
    Hold,
}

/// Builds the ladder for one opportunity.
///
/// `cost_minor` is the costed trip. `None` means the trip could not be costed,
/// and the ladder is then built from the walk-away figure the opportunity row
/// carries — which is enough to draft a counter and never enough to accept,
/// because [`evaluate_terms`] refuses an uncosted acceptance separately.
#[must_use]
pub fn terms_ladder(
    snapshot: LiveOpportunitySnapshot,
    policy: LiveOpportunityPolicy,
    cost_minor: i64,
) -> TermsLadder {
    let tier = StrategicTier::from_basis_points(snapshot.strategic_value_basis_points, policy);
    // The bounded loss tolerance drops the floor and nothing else: a festival
    // slot may run a stated loss, a club date on a Tuesday may not, and neither
    // gets to skip the contract or exclusivity refusals.
    let tolerance = if matches!(tier, StrategicTier::Landmark) {
        policy.max_strategic_negative_margin_minor
    } else {
        0
    };
    let walk_away_minor = cost_minor
        .saturating_add(policy.minimum_margin_minor)
        .saturating_add(snapshot.application_fee_minor)
        .saturating_sub(tolerance)
        .max(0);
    // Uplifts are applied to the floor rather than to the offer. Anchoring on
    // what the promoter said would let a deliberately low first offer drag the
    // band's own target down with it, which is the oldest trick in booking.
    let target_minor = uplift(walk_away_minor, policy.target_uplift_basis_points);
    let opening_ask_minor = uplift(target_minor, policy.opening_ask_uplift_basis_points);
    TermsLadder {
        walk_away_minor,
        target_minor,
        opening_ask_minor,
    }
}

fn uplift(value: i64, basis_points: u16) -> i64 {
    let scaled = i128::from(value) * i128::from(10_000 + u32::from(basis_points)) / 10_000;
    i64::try_from(scaled).unwrap_or(i64::MAX)
}

/// The refusals that hold at every autonomy level.
///
/// Returned before any arithmetic is looked at, because a fee that clears the
/// floor on a show requiring a signed contract is still a show requiring a
/// signed contract.
#[must_use]
pub fn terms_refusal(
    snapshot: LiveOpportunitySnapshot,
    policy: LiveOpportunityPolicy,
    score: u16,
    now: OffsetDateTime,
) -> Option<TermsRefusal> {
    if snapshot.requires_contract {
        return Some(TermsRefusal::RequiresContract);
    }
    if snapshot.exclusive {
        return Some(TermsRefusal::Exclusive);
    }
    let free_date = snapshot
        .event_starts_at
        .is_some_and(|starts_at| starts_at.unix_timestamp() > now.unix_timestamp());
    if !free_date {
        return Some(TermsRefusal::DateNotFree);
    }
    if !snapshot.costed_from_logistics {
        return Some(TermsRefusal::CostInsufficient);
    }
    let filled = snapshot
        .committed_shows_year
        .saturating_add(snapshot.pipeline_shows_year);
    if filled >= snapshot.annual_stretch {
        return Some(TermsRefusal::PastAnnualStretch);
    }
    // Inside the stretch band, the operator's own bar for a stretch show
    // applies. Landmark is exempt from the *scarcity ramp* by Phase 8b, not
    // from the operator's stated floor for filling their last slots.
    if filled >= snapshot.annual_target
        && u32::from(score) * 100 < u32::from(snapshot.stretch_minimum_score_basis_points)
    {
        return Some(TermsRefusal::StretchScoreTooLow);
    }
    let _ = policy;
    None
}

/// Decides the next move on one negotiation.
///
/// Order is the rule. A settled negotiation is never revisited; an expired
/// window beats every offer, because terms agreed after the promoter stopped
/// waiting are not terms; a refusal beats an offer that clears, because money
/// does not fix a contract; and a counter already sent holds rather than
/// counter-offering against itself.
#[must_use]
pub fn evaluate_terms(
    terms: TermsSnapshot,
    snapshot: LiveOpportunitySnapshot,
    policy: LiveOpportunityPolicy,
    score: u16,
    now: OffsetDateTime,
) -> TermsDecision {
    if terms.state.settled() {
        return TermsDecision::Hold;
    }
    if now.unix_timestamp() >= terms.responds_by.unix_timestamp() {
        return TermsDecision::Expire;
    }
    if let Some(reason) = terms_refusal(snapshot, policy, score, now) {
        return TermsDecision::Decline { reason };
    }
    // Waiting on the promoter. Sending a second ask before the first is
    // answered is negotiating against the band's own last offer.
    if matches!(terms.state, TermsState::Countered) {
        return TermsDecision::Hold;
    }
    // Re-read the *current* margin rather than trusting the frozen ladder
    // alone. A ladder built when the trip looked cheap must not talk the agent
    // into a show that no longer clears.
    let clears_now = net_margin(snapshot).is_some_and(|margin| margin >= 0);
    if terms.offered_fee_minor >= terms.ladder.target_minor && clears_now {
        return TermsDecision::Accept {
            fee_minor: terms.offered_fee_minor,
        };
    }
    if terms.offered_fee_minor < terms.ladder.walk_away_minor
        && terms.counter_rounds >= policy.max_counter_rounds
    {
        // Asked as many times as the operator allows and still under the floor.
        // Continuing is the band talking to itself.
        return TermsDecision::Decline {
            reason: TermsRefusal::BelowFloor,
        };
    }
    if terms.counter_rounds >= policy.max_counter_rounds {
        // Between the floor and the target, out of asks. Clearing the floor is
        // a show worth playing, and holding out for the target until the
        // promoter walks is not.
        return if clears_now {
            TermsDecision::Accept {
                fee_minor: terms.offered_fee_minor,
            }
        } else {
            TermsDecision::Decline {
                reason: TermsRefusal::BelowFloor,
            }
        };
    }
    let round = terms.counter_rounds.saturating_add(1);
    TermsDecision::Counter {
        ask_minor: counter_ask(terms, round, policy),
        round,
    }
}

/// What to ask for on this round.
///
/// The first ask is the opening one. Every later ask concedes a share of the
/// gap between the last ask and the target, and never falls below the target:
/// conceding past the number the band actually wanted turns a negotiation into
/// a discount.
fn counter_ask(terms: TermsSnapshot, round: u8, policy: LiveOpportunityPolicy) -> i64 {
    let Some(previous) = terms.countered_fee_minor else {
        return terms.ladder.opening_ask_minor;
    };
    let remaining = policy.max_counter_rounds.saturating_sub(round) + 1;
    let gap = previous.saturating_sub(terms.ladder.target_minor).max(0);
    let concession = gap / i64::from(remaining.max(1));
    previous
        .saturating_sub(concession)
        .max(terms.ladder.target_minor)
}

fn net_margin(snapshot: LiveOpportunitySnapshot) -> Option<i64> {
    if !snapshot.costed_from_logistics {
        return None;
    }
    Some(
        snapshot
            .expected_fee_minor
            .saturating_sub(snapshot.estimated_cost_minor)
            .saturating_sub(snapshot.application_fee_minor),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{autonomy::Confidence, live_opportunities::LiveOpportunityKind};

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_780_000_000).expect("valid timestamp")
    }

    fn opportunity() -> LiveOpportunitySnapshot {
        LiveOpportunitySnapshot {
            opportunity_id: TeamOpportunityId::new(),
            kind: LiveOpportunityKind::SupportSlot,
            active: true,
            verified_destination: true,
            auto_submission_capable: true,
            fit_basis_points: 9_000,
            reputation_basis_points: 8_000,
            evidence_confidence: Confidence::saturating_from_basis_points(9_000),
            expected_fee_minor: 300_000,
            estimated_cost_minor: 150_000,
            application_fee_minor: 0,
            requires_contract: false,
            exclusive: false,
            deadline: None,
            event_starts_at: Some(now() + time::Duration::days(60)),
            travel_band: None,
            costed_from_logistics: true,
            committed_shows_year: 2,
            pipeline_shows_year: 1,
            annual_target: 15,
            annual_stretch: 20,
            stretch_minimum_score_basis_points: 9_000,
            far_shot_minimum_score_basis_points: 9_000,
            prefer_weekend_one_shots: false,
            already_applied: false,
            strategic_value_basis_points: 0,
        }
    }

    fn terms(ladder: TermsLadder, offered: i64) -> TermsSnapshot {
        TermsSnapshot {
            opportunity_id: TeamOpportunityId::new(),
            state: TermsState::Proposed,
            offered_fee_minor: offered,
            ladder,
            countered_fee_minor: None,
            counter_rounds: 0,
            responds_by: now() + time::Duration::days(7),
        }
    }

    #[test]
    fn the_ladder_climbs_and_the_floor_is_cost_plus_margin_plus_application() {
        let policy = LiveOpportunityPolicy {
            minimum_margin_minor: 50_000,
            ..LiveOpportunityPolicy::default()
        };
        let mut snapshot = opportunity();
        snapshot.application_fee_minor = 10_000;
        let ladder = terms_ladder(snapshot, policy, 150_000);
        assert_eq!(ladder.walk_away_minor, 210_000);
        assert!(ladder.target_minor > ladder.walk_away_minor);
        assert!(ladder.opening_ask_minor > ladder.target_minor);
    }

    #[test]
    fn only_a_landmark_slot_may_lower_its_own_floor() {
        let policy = LiveOpportunityPolicy {
            minimum_margin_minor: 50_000,
            max_strategic_negative_margin_minor: 40_000,
            ..LiveOpportunityPolicy::default()
        };
        let standard = terms_ladder(opportunity(), policy, 150_000);
        let mut landmark_snapshot = opportunity();
        landmark_snapshot.strategic_value_basis_points = 9_000;
        let landmark = terms_ladder(landmark_snapshot, policy, 150_000);
        assert_eq!(standard.walk_away_minor, 200_000);
        assert_eq!(landmark.walk_away_minor, 160_000);
    }

    #[test]
    fn the_opening_ask_is_anchored_on_the_floor_not_on_the_offer() {
        // Anchoring on the promoter's number lets a deliberately low first
        // offer drag the band's own target down with it.
        let policy = LiveOpportunityPolicy::default();
        let ladder = terms_ladder(opportunity(), policy, 150_000);
        let low = evaluate_terms(terms(ladder, 1), opportunity(), policy, 80, now());
        let high = evaluate_terms(
            terms(ladder, ladder.walk_away_minor),
            opportunity(),
            policy,
            80,
            now(),
        );
        assert_eq!(
            low, high,
            "the first ask does not depend on the first offer"
        );
        assert!(matches!(
            low,
            TermsDecision::Counter {
                ask_minor,
                round: 1
            } if ask_minor == ladder.opening_ask_minor
        ));
    }

    #[test]
    fn an_offer_at_or_above_target_is_accepted() {
        let policy = LiveOpportunityPolicy::default();
        let ladder = terms_ladder(opportunity(), policy, 150_000);
        let decision = evaluate_terms(
            terms(ladder, ladder.target_minor),
            opportunity(),
            policy,
            80,
            now(),
        );
        assert_eq!(
            decision,
            TermsDecision::Accept {
                fee_minor: ladder.target_minor
            }
        );
    }

    #[test]
    fn money_never_buys_a_contract_or_an_exclusivity_clause() {
        let policy = LiveOpportunityPolicy::default();
        let ladder = terms_ladder(opportunity(), policy, 150_000);
        for (mutate, expected) in [
            (
                (|snapshot: &mut LiveOpportunitySnapshot| snapshot.requires_contract = true)
                    as fn(&mut LiveOpportunitySnapshot),
                TermsRefusal::RequiresContract,
            ),
            (
                |snapshot: &mut LiveOpportunitySnapshot| snapshot.exclusive = true,
                TermsRefusal::Exclusive,
            ),
            (
                |snapshot: &mut LiveOpportunitySnapshot| snapshot.event_starts_at = None,
                TermsRefusal::DateNotFree,
            ),
            (
                |snapshot: &mut LiveOpportunitySnapshot| snapshot.costed_from_logistics = false,
                TermsRefusal::CostInsufficient,
            ),
            (
                |snapshot: &mut LiveOpportunitySnapshot| snapshot.pipeline_shows_year = 30,
                TermsRefusal::PastAnnualStretch,
            ),
        ] {
            let mut snapshot = opportunity();
            mutate(&mut snapshot);
            // An offer far above the opening ask, so nothing here is about money.
            let generous = terms(ladder, ladder.opening_ask_minor * 10);
            assert_eq!(
                evaluate_terms(generous, snapshot, policy, 99, now()),
                TermsDecision::Decline { reason: expected }
            );
        }
    }

    #[test]
    fn a_stretch_slot_holds_the_operators_own_bar() {
        let policy = LiveOpportunityPolicy::default();
        let ladder = terms_ladder(opportunity(), policy, 150_000);
        let mut snapshot = opportunity();
        snapshot.committed_shows_year = 16;
        snapshot.pipeline_shows_year = 0;
        assert_eq!(
            evaluate_terms(
                terms(ladder, ladder.target_minor),
                snapshot,
                policy,
                80,
                now()
            ),
            TermsDecision::Decline {
                reason: TermsRefusal::StretchScoreTooLow
            }
        );
        // Clear the bar and the same slot is takeable.
        assert!(matches!(
            evaluate_terms(
                terms(ladder, ladder.target_minor),
                snapshot,
                policy,
                95,
                now()
            ),
            TermsDecision::Accept { .. }
        ));
    }

    #[test]
    fn a_counter_already_sent_waits_for_an_answer() {
        let policy = LiveOpportunityPolicy::default();
        let ladder = terms_ladder(opportunity(), policy, 150_000);
        let mut waiting = terms(ladder, ladder.walk_away_minor);
        waiting.state = TermsState::Countered;
        waiting.countered_fee_minor = Some(ladder.opening_ask_minor);
        waiting.counter_rounds = 1;
        assert_eq!(
            evaluate_terms(waiting, opportunity(), policy, 80, now()),
            TermsDecision::Hold,
            "a second ask before the first is answered negotiates against ourselves"
        );
    }

    #[test]
    fn each_round_concedes_toward_the_target_and_never_past_it() {
        let policy = LiveOpportunityPolicy::default();
        let ladder = terms_ladder(opportunity(), policy, 150_000);
        let mut round = terms(ladder, ladder.walk_away_minor);
        let mut previous = ladder.opening_ask_minor;
        for expected_round in 2..=policy.max_counter_rounds {
            round.countered_fee_minor = Some(previous);
            round.counter_rounds = expected_round - 1;
            round.state = TermsState::Proposed;
            let TermsDecision::Counter {
                ask_minor,
                round: n,
            } = evaluate_terms(round, opportunity(), policy, 80, now())
            else {
                panic!("still has asks left");
            };
            assert_eq!(n, expected_round);
            assert!(ask_minor <= previous, "each ask concedes");
            assert!(
                ask_minor >= ladder.target_minor,
                "conceding past the target turns a negotiation into a discount"
            );
            previous = ask_minor;
        }
    }

    #[test]
    fn out_of_asks_the_agent_takes_what_clears_and_refuses_what_does_not() {
        let policy = LiveOpportunityPolicy::default();
        let ladder = terms_ladder(opportunity(), policy, 150_000);
        let mut exhausted = terms(ladder, ladder.walk_away_minor);
        exhausted.counter_rounds = policy.max_counter_rounds;
        exhausted.countered_fee_minor = Some(ladder.target_minor);
        // Holding out for the target until the promoter walks is not judgement.
        assert!(matches!(
            evaluate_terms(exhausted, opportunity(), policy, 80, now()),
            TermsDecision::Accept { .. }
        ));
        let mut short = opportunity();
        short.expected_fee_minor = 10;
        assert_eq!(
            evaluate_terms(exhausted, short, policy, 80, now()),
            TermsDecision::Decline {
                reason: TermsRefusal::BelowFloor
            }
        );
    }

    #[test]
    fn a_closed_window_beats_every_offer() {
        let policy = LiveOpportunityPolicy::default();
        let ladder = terms_ladder(opportunity(), policy, 150_000);
        let mut late = terms(ladder, ladder.opening_ask_minor);
        late.responds_by = now() - time::Duration::hours(1);
        assert_eq!(
            evaluate_terms(late, opportunity(), policy, 80, now()),
            TermsDecision::Expire,
            "terms agreed after the promoter stopped waiting are not terms"
        );
    }

    #[test]
    fn a_settled_negotiation_is_never_revisited() {
        let policy = LiveOpportunityPolicy::default();
        let ladder = terms_ladder(opportunity(), policy, 150_000);
        for state in [
            TermsState::Accepted,
            TermsState::Declined,
            TermsState::Expired,
        ] {
            let mut settled = terms(ladder, ladder.opening_ask_minor);
            settled.state = state;
            assert!(state.settled());
            assert_eq!(
                evaluate_terms(settled, opportunity(), policy, 80, now()),
                TermsDecision::Hold
            );
            assert_eq!(TermsState::parse(state.as_str()), Some(state));
        }
    }
}
