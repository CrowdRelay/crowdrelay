//! Festival, showcase, review-contest and support-slot opportunity context.
//!
//! Discovery adapters report facts. The domain scores economic/reputational fit
//! and decides whether an application may be sent automatically. Paid,
//! contractual or exclusive applications are never auto-submitted.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{TeamOpportunityId, autonomy::Confidence};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveOpportunityKind {
    Festival,
    Showcase,
    ReviewContest,
    SupportSlot,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveTravelBand {
    Poland,
    EastGermany,
    CzechiaSlovakia,
    FarShot,
}

impl LiveTravelBand {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Poland => "poland",
            Self::EastGermany => "east_germany",
            Self::CzechiaSlovakia => "czechia_slovakia",
            Self::FarShot => "far_shot",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct LiveOpportunitySnapshot {
    pub opportunity_id: TeamOpportunityId,
    pub kind: LiveOpportunityKind,
    pub active: bool,
    pub verified_destination: bool,
    pub auto_submission_capable: bool,
    pub fit_basis_points: u16,
    pub reputation_basis_points: u16,
    pub evidence_confidence: Confidence,
    pub expected_fee_minor: i64,
    pub estimated_cost_minor: i64,
    pub application_fee_minor: i64,
    pub requires_contract: bool,
    pub exclusive: bool,
    pub deadline: Option<OffsetDateTime>,
    /// Calendar/travel facts and manager policy are folded into the snapshot by
    /// the infrastructure adapter so the domain owns the actual booking gate.
    pub event_starts_at: Option<OffsetDateTime>,
    pub travel_band: Option<LiveTravelBand>,
    /// True when [`crate::tour_economics`] costed this trip from real inputs
    /// rather than the number falling back to whatever was typed in.
    ///
    /// An uncosted show can still be prepared for a human — that is the whole
    /// point of preparing — but it can never be submitted automatically. The
    /// band does not commit to a 500 km drive on an unverified cost.
    pub costed_from_logistics: bool,
    pub committed_shows_year: u16,
    /// Opportunities already in flight for the same window: applications
    /// `submitted` or `replied`, plus venue conversations whose newest reply
    /// was positive or booked. None of this is on the calendar yet, so
    /// `committed_shows_year` alone reads a year with ten live negotiations as
    /// empty and keeps finding more. Counted at full weight and self-correcting
    /// — when a negotiation dies it leaves the pipeline and the bar it raised
    /// comes back down next cycle, no decay curve required.
    pub pipeline_shows_year: u16,
    pub annual_target: u16,
    pub annual_stretch: u16,
    pub stretch_minimum_score_basis_points: u16,
    pub far_shot_minimum_score_basis_points: u16,
    pub prefer_weekend_one_shots: bool,
    pub already_applied: bool,
    /// Operator-confirmed value, `0..=10_000`. A name match against a landmark
    /// promoter or festival list is a suggestion, never an automatic grant —
    /// "Festival" in a title means nothing on its own, and this field is never
    /// written from discovery text alone.
    pub strategic_value_basis_points: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveOpportunityDiscovery<'a> {
    pub title: &'a str,
    pub summary: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LiveOpportunityDiscoveryAssessment {
    pub kind: LiveOpportunityKind,
    pub fit_basis_points: u16,
    pub reputation_basis_points: u16,
    pub confidence: Confidence,
}

#[must_use]
pub fn evaluate_live_opportunity_discovery(
    discovery: &LiveOpportunityDiscovery<'_>,
) -> Option<LiveOpportunityDiscoveryAssessment> {
    let text = format!("{} {}", discovery.title, discovery.summary).to_lowercase();
    let kind = if text.contains("festival") {
        LiveOpportunityKind::Festival
    } else if text.contains("showcase") || text.contains("przegląd") {
        LiveOpportunityKind::Showcase
    } else if text.contains("support") {
        LiveOpportunityKind::SupportSlot
    } else if text.contains("review") || text.contains("contest") || text.contains("konkurs") {
        LiveOpportunityKind::ReviewContest
    } else {
        return None;
    };
    Some(LiveOpportunityDiscoveryAssessment {
        kind,
        fit_basis_points: 7_000,
        reputation_basis_points: 5_000,
        confidence: Confidence::saturating_from_basis_points(6_500),
    })
}

/// Operator-owned live calendar guardrails. Google Sheets may be the editing
/// surface, but this validated value is persisted in CrowdRelay before use.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BookingManagerPolicy {
    pub annual_target: u16,
    pub annual_stretch: u16,
    pub stretch_minimum_score_basis_points: u16,
    pub prefer_weekend_one_shots: bool,
    pub priority_markets: Vec<String>,
    pub far_shot_minimum_score_basis_points: u16,
}

impl Default for BookingManagerPolicy {
    fn default() -> Self {
        Self {
            annual_target: 15,
            annual_stretch: 20,
            stretch_minimum_score_basis_points: 9_000,
            prefer_weekend_one_shots: true,
            priority_markets: vec!["PL".into(), "DE-EAST".into(), "CZ".into(), "SK".into()],
            far_shot_minimum_score_basis_points: 9_000,
        }
    }
}

impl BookingManagerPolicy {
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.annual_target > 0
            && self.annual_target <= 60
            && self.annual_stretch >= self.annual_target
            && self.annual_stretch <= 60
            && self.stretch_minimum_score_basis_points <= 10_000
            && self.far_shot_minimum_score_basis_points <= 10_000
            && !self.priority_markets.is_empty()
            && self.priority_markets.len() <= 12
            && self.priority_markets.iter().all(|market| {
                !market.trim().is_empty()
                    && market.len() <= 24
                    && market.bytes().all(|byte| {
                        byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'-'
                    })
            })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct LiveOpportunityPolicy {
    pub minimum_score: u16,
    pub minimum_auto_score: u16,
    pub max_auto_application_fee_minor: i64,
    pub max_auto_negative_margin_minor: i64,
    /// Points added to `minimum_score` per slot the pipeline sits past
    /// `annual_target`. Scarcity raises the bar rather than closing the door:
    /// ten in the pipeline against a target of fifteen means only genuinely
    /// strong offers still get through, without an outright cutoff.
    pub scarcity_step: u16,
    /// At or above this, an opportunity is Landmark: exempt from the scarcity
    /// ramp, allowed the bounded loss below, and escalated to a human rather
    /// than held when the year is full — never silently dropped.
    pub landmark_threshold_basis_points: u16,
    /// At or above this and below Landmark, an opportunity is Notable: the
    /// score weighting counts it for more, nothing else changes.
    pub notable_threshold_basis_points: u16,
    /// Applies only at the Landmark tier. A festival slot may run a stated
    /// loss up to this bound; a club date on a Tuesday may not. Bounded, never
    /// open-ended.
    pub max_strategic_negative_margin_minor: i64,
    /// What the band must clear above the costed trip for a show to be worth
    /// playing. Mirrors the tour-economics figure so the negotiation floor and
    /// the economics verdict cannot drift apart.
    pub minimum_margin_minor: i64,
    /// How far above the walk-away floor the target sits. The target is the fee
    /// that makes the show clearly worth playing rather than merely not a loss.
    pub target_uplift_basis_points: u16,
    /// How far above the target the opening ask sits. An opening ask that is
    /// already the target leaves the band negotiating down from the number they
    /// actually wanted.
    pub opening_ask_uplift_basis_points: u16,
    /// Counters the agent may make before it takes what clears or walks. A
    /// third ask to somebody who has improved their offer twice is the band
    /// talking itself out of a show.
    pub max_counter_rounds: u8,
}

impl Default for LiveOpportunityPolicy {
    fn default() -> Self {
        Self {
            minimum_score: 65,
            // Strategic value now carries 25 of the 100 points, so a Standard
            // opportunity — one nobody has confirmed any prestige on — tops out
            // at 75. Set above the old 82 and this becomes unreachable for
            // every ordinary booking, which would silently stop the most
            // common case from ever auto-submitting again.
            minimum_auto_score: 70,
            max_auto_application_fee_minor: 0,
            max_auto_negative_margin_minor: 0,
            scarcity_step: 2,
            landmark_threshold_basis_points: 8_500,
            notable_threshold_basis_points: 6_000,
            max_strategic_negative_margin_minor: 0,
            // Zero means "not configured", and a zero floor is reported as a
            // refusal at negotiation time rather than as a free show.
            minimum_margin_minor: 0,
            target_uplift_basis_points: 2_500,
            opening_ask_uplift_basis_points: 2_000,
            max_counter_rounds: 2,
        }
    }
}

/// Where a strategic value lands. A Mystic or Pol'and'Rock slot is worth
/// playing at break-even, and a plain score cannot express that: this is what
/// lets prestige outrank money at the top of the range without pretending
/// every booking should.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategicTier {
    Standard,
    Notable,
    Landmark,
}

impl StrategicTier {
    #[must_use]
    pub const fn from_basis_points(value: u16, policy: LiveOpportunityPolicy) -> Self {
        if value >= policy.landmark_threshold_basis_points {
            Self::Landmark
        } else if value >= policy.notable_threshold_basis_points {
            Self::Notable
        } else {
            Self::Standard
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Notable => "notable",
            Self::Landmark => "landmark",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiveOpportunityDecision {
    Hold,
    PrepareForApproval {
        score: u16,
        confidence: Confidence,
    },
    SubmitAutomatically {
        score: u16,
        confidence: Confidence,
    },
    /// A Landmark opportunity at or beyond the annual stretch. Never dropped by
    /// a budget rule: a full year is a reason to ask, not a reason to throw
    /// away the best offer of it.
    EscalateLandmark {
        score: u16,
        confidence: Confidence,
    },
}

#[must_use]
pub fn live_opportunity_score(snapshot: LiveOpportunitySnapshot) -> u16 {
    let fit = u32::from(snapshot.fit_basis_points) * 30 / 10_000;
    let strategic = u32::from(snapshot.strategic_value_basis_points) * 25 / 10_000;
    let reputation = u32::from(snapshot.reputation_basis_points) * 15 / 10_000;
    let confidence = u32::from(snapshot.evidence_confidence.basis_points()) * 15 / 10_000;
    let economics = economics_score(snapshot);
    u16::try_from((fit + strategic + reputation + confidence + economics).min(100)).unwrap_or(100)
}

#[must_use]
pub fn evaluate_live_opportunity(
    snapshot: LiveOpportunitySnapshot,
    policy: LiveOpportunityPolicy,
    now: OffsetDateTime,
) -> LiveOpportunityDecision {
    if !valid_policy(policy)
        || !snapshot.active
        || !snapshot.verified_destination
        || snapshot.already_applied
        || snapshot.expected_fee_minor < 0
        || snapshot.estimated_cost_minor < 0
        || snapshot.application_fee_minor < 0
        || snapshot.deadline.is_some_and(|deadline| deadline <= now)
    {
        return LiveOpportunityDecision::Hold;
    }

    let score = live_opportunity_score(snapshot);
    let tier = StrategicTier::from_basis_points(snapshot.strategic_value_basis_points, policy);
    let confidence = Confidence::saturating_from_basis_points(
        7_500_u16
            .saturating_add(
                score
                    .saturating_sub(policy.minimum_score)
                    .saturating_mul(100)
                    .min(2_500),
            )
            .min(10_000),
    );

    match annual_budget_verdict(snapshot, policy, tier, score) {
        AnnualBudget::PastStretch => {
            // Never dropped by a budget rule: a full year is a reason to ask,
            // not a reason to throw away the best offer of it.
            return if matches!(tier, StrategicTier::Landmark) {
                LiveOpportunityDecision::EscalateLandmark { score, confidence }
            } else {
                LiveOpportunityDecision::Hold
            };
        }
        AnnualBudget::BelowEffectiveMinimum => return LiveOpportunityDecision::Hold,
        AnnualBudget::Clears => {}
    }
    if score < policy.minimum_score {
        return LiveOpportunityDecision::Hold;
    }
    if matches!(snapshot.travel_band, Some(LiveTravelBand::FarShot))
        && score.saturating_mul(100) < snapshot.far_shot_minimum_score_basis_points
    {
        return LiveOpportunityDecision::Hold;
    }

    if may_auto_submit(snapshot, policy, tier, score) {
        LiveOpportunityDecision::SubmitAutomatically { score, confidence }
    } else {
        LiveOpportunityDecision::PrepareForApproval { score, confidence }
    }
}

#[must_use]
fn economics_score(snapshot: LiveOpportunitySnapshot) -> u32 {
    if !snapshot.costed_from_logistics {
        // No costed trip, no economics points. Scoring an unknown cost as
        // break-even is how an uncosted gig outranks a costed profitable one.
        return 0;
    }
    let margin = net_margin_minor(snapshot);
    if margin >= 100_000 {
        15
    } else if margin >= 0 {
        12
    } else if margin >= -30_000 {
        6
    } else {
        0
    }
}

#[must_use]
fn may_auto_submit(
    snapshot: LiveOpportunitySnapshot,
    policy: LiveOpportunityPolicy,
    tier: StrategicTier,
    score: u16,
) -> bool {
    // The bounded loss tolerance applies only at the Landmark floor: a
    // festival slot may run a stated loss, a club date on a Tuesday may not.
    let margin_floor = if matches!(tier, StrategicTier::Landmark) {
        -policy.max_strategic_negative_margin_minor
    } else {
        -policy.max_auto_negative_margin_minor
    };
    score >= policy.minimum_auto_score
        && schedule_allows_auto_submit(snapshot, score)
        && snapshot.auto_submission_capable
        && snapshot.costed_from_logistics
        && snapshot.application_fee_minor <= policy.max_auto_application_fee_minor
        && net_margin_minor(snapshot) >= margin_floor
        && !snapshot.requires_contract
        && !snapshot.exclusive
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AnnualBudget {
    /// The pipeline has run past the annual stretch. Absolute for every tier
    /// except Landmark, which is handled by the caller.
    PastStretch,
    /// Past the annual target, the minimum score to even prepare climbs with
    /// each slot the pipeline has already consumed. This opportunity did not
    /// clear that raised bar.
    BelowEffectiveMinimum,
    Clears,
}

/// Committed shows plus the pipeline already in flight, read against the
/// operator's annual target and stretch.
///
/// Landmark is exempt from the scarcity ramp entirely: prestige is the reason
/// the tier exists, and a rising bar exists to protect ordinary bookings from
/// each other, not to filter out the best offer of the year.
#[must_use]
fn annual_budget_verdict(
    snapshot: LiveOpportunitySnapshot,
    policy: LiveOpportunityPolicy,
    tier: StrategicTier,
    score: u16,
) -> AnnualBudget {
    if snapshot.annual_target == 0 || snapshot.annual_stretch < snapshot.annual_target {
        return AnnualBudget::BelowEffectiveMinimum;
    }
    // Counted at full weight and self-correcting: when a negotiation dies it
    // leaves `submitted`/`replied` or a positive reply cools, the pipeline
    // count drops, and the bar comes back down on the next cycle.
    let filled = snapshot
        .committed_shows_year
        .saturating_add(snapshot.pipeline_shows_year);
    if filled >= snapshot.annual_stretch {
        return AnnualBudget::PastStretch;
    }
    if matches!(tier, StrategicTier::Landmark) {
        return AnnualBudget::Clears;
    }
    if filled <= snapshot.annual_target {
        return AnnualBudget::Clears;
    }
    // Scarcity raises the bar rather than closing the door: each slot past
    // target adds `scarcity_step` to the minimum score required, capped at the
    // operator's own stretch bar so the ramp is never stricter than the ceiling
    // that already existed for it.
    let slots_past = filled - snapshot.annual_target;
    let ramp = policy.scarcity_step.saturating_mul(slots_past);
    let stretch_bar = (snapshot.stretch_minimum_score_basis_points / 100).min(100);
    let effective_minimum = policy
        .minimum_score
        .saturating_add(ramp)
        .min(100)
        .min(stretch_bar.max(policy.minimum_score));
    if score < effective_minimum {
        AnnualBudget::BelowEffectiveMinimum
    } else {
        AnnualBudget::Clears
    }
}

#[must_use]
fn schedule_allows_auto_submit(snapshot: LiveOpportunitySnapshot, score: u16) -> bool {
    let Some(starts_at) = snapshot.event_starts_at else {
        // Unknown calendar date is safe to prepare, but not safe to commit to automatically.
        return false;
    };
    if !snapshot.prefer_weekend_one_shots {
        return true;
    }
    let weekday = starts_at.weekday();
    let weekend = matches!(
        weekday,
        time::Weekday::Friday | time::Weekday::Saturday | time::Weekday::Sunday
    );
    weekend || score.saturating_mul(100) >= snapshot.stretch_minimum_score_basis_points
}

#[must_use]
const fn net_margin_minor(snapshot: LiveOpportunitySnapshot) -> i64 {
    snapshot
        .expected_fee_minor
        .saturating_sub(snapshot.estimated_cost_minor)
        .saturating_sub(snapshot.application_fee_minor)
}

#[must_use]
const fn valid_policy(policy: LiveOpportunityPolicy) -> bool {
    policy.minimum_score <= policy.minimum_auto_score
        && policy.minimum_auto_score <= 100
        && policy.max_auto_application_fee_minor >= 0
        && policy.max_auto_negative_margin_minor >= 0
        && policy.notable_threshold_basis_points <= policy.landmark_threshold_basis_points
        && policy.landmark_threshold_basis_points <= 10_000
        && policy.max_strategic_negative_margin_minor >= 0
        && policy.minimum_margin_minor >= 0
        && policy.max_counter_rounds >= 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    fn snapshot() -> LiveOpportunitySnapshot {
        LiveOpportunitySnapshot {
            opportunity_id: TeamOpportunityId::new(),
            kind: LiveOpportunityKind::Festival,
            active: true,
            verified_destination: true,
            auto_submission_capable: true,
            costed_from_logistics: true,
            fit_basis_points: 10_000,
            reputation_basis_points: 10_000,
            evidence_confidence: Confidence::saturating_from_basis_points(10_000),
            expected_fee_minor: 400_000,
            estimated_cost_minor: 100_000,
            application_fee_minor: 0,
            requires_contract: false,
            exclusive: false,
            deadline: Some(now() + Duration::days(20)),
            event_starts_at: Some(now() + Duration::days(24)),
            travel_band: Some(LiveTravelBand::Poland),
            committed_shows_year: 8,
            pipeline_shows_year: 0,
            annual_target: 15,
            annual_stretch: 20,
            stretch_minimum_score_basis_points: 9_000,
            far_shot_minimum_score_basis_points: 9_000,
            prefer_weekend_one_shots: false,
            already_applied: false,
            strategic_value_basis_points: 0,
        }
    }

    #[test]
    fn excellent_free_reversible_opportunity_may_auto_submit() {
        // Perfect fit, reputation and confidence, no strategic value declared:
        // this is the ceiling an ordinary booking can reach (75), and it must
        // still clear the auto bar. If it did not, no everyday show could ever
        // auto-submit again.
        assert_eq!(live_opportunity_score(snapshot()), 75);
        assert!(matches!(
            evaluate_live_opportunity(snapshot(), LiveOpportunityPolicy::default(), now()),
            LiveOpportunityDecision::SubmitAutomatically { .. }
        ));
    }

    #[test]
    fn paid_or_contractual_application_requires_approval() {
        let mut candidate = snapshot();
        candidate.application_fee_minor = 5_000;
        candidate.requires_contract = true;
        assert!(matches!(
            evaluate_live_opportunity(candidate, LiveOpportunityPolicy::default(), now()),
            LiveOpportunityDecision::PrepareForApproval { .. }
        ));
    }

    #[test]
    fn scarcity_raises_the_bar_rather_than_closing_the_door() {
        // Fifteen committed, three more already at "submitted" or "replied":
        // the pipeline is what the plan calls "ten promising negotiations" —
        // real capacity the calendar count alone cannot see. Three slots past
        // target at the default step of two raises the floor from 65 to 71.
        let mut candidate = snapshot();
        candidate.committed_shows_year = 15;
        candidate.pipeline_shows_year = 3;
        candidate.fit_basis_points = 7_000;
        candidate.reputation_basis_points = 6_000;
        // fit 21 + reputation 9 + confidence 15 + economics 15 = 60, under the
        // raised bar of 71.
        assert_eq!(
            evaluate_live_opportunity(candidate, LiveOpportunityPolicy::default(), now()),
            LiveOpportunityDecision::Hold
        );
    }

    #[test]
    fn pipeline_alone_raises_the_bar_even_when_committed_is_low() {
        // The gap 8a exists for: a mostly-empty calendar with a full pipeline
        // must not read as an empty year and keep finding more.
        let mut candidate = snapshot();
        candidate.committed_shows_year = 2;
        candidate.pipeline_shows_year = 16;
        candidate.fit_basis_points = 7_000;
        candidate.reputation_basis_points = 6_000;
        assert_eq!(
            evaluate_live_opportunity(candidate, LiveOpportunityPolicy::default(), now()),
            LiveOpportunityDecision::Hold
        );
    }

    #[test]
    fn a_show_strong_enough_still_clears_the_raised_bar() {
        let mut candidate = snapshot();
        candidate.committed_shows_year = 15;
        candidate.pipeline_shows_year = 3;
        assert!(!matches!(
            evaluate_live_opportunity(candidate, LiveOpportunityPolicy::default(), now()),
            LiveOpportunityDecision::Hold
        ));
    }

    #[test]
    fn a_negotiation_that_dies_lets_the_bar_come_back_down() {
        // Self-correcting, no decay curve: dropping the pipeline count is the
        // whole mechanism. A 66-point show clears the ordinary floor (65) but
        // not the ramped one (71) three slots past target.
        let mut candidate = snapshot();
        candidate.committed_shows_year = 15;
        candidate.fit_basis_points = 8_500;
        candidate.reputation_basis_points = 7_500;
        assert_eq!(live_opportunity_score(candidate), 66);

        candidate.pipeline_shows_year = 3;
        assert_eq!(
            evaluate_live_opportunity(candidate, LiveOpportunityPolicy::default(), now()),
            LiveOpportunityDecision::Hold
        );

        // The negotiation dies, the pipeline count drops, and the same show
        // clears on the next cycle without anything else changing.
        candidate.pipeline_shows_year = 0;
        assert!(!matches!(
            evaluate_live_opportunity(candidate, LiveOpportunityPolicy::default(), now()),
            LiveOpportunityDecision::Hold
        ));
    }

    #[test]
    fn the_ramp_never_exceeds_the_operators_own_stretch_bar() {
        let policy = LiveOpportunityPolicy {
            scarcity_step: 50,
            ..LiveOpportunityPolicy::default()
        };
        let mut candidate = snapshot();
        candidate.committed_shows_year = 15;
        candidate.pipeline_shows_year = 1;
        candidate.stretch_minimum_score_basis_points = 7_200;
        // One slot past target at a step of 50 would demand 115 — impossible.
        // The ramp is capped at the operator's own stretch bar, 72.
        assert_eq!(
            annual_budget_verdict(candidate, policy, StrategicTier::Standard, 73),
            AnnualBudget::Clears
        );
        assert_eq!(
            annual_budget_verdict(candidate, policy, StrategicTier::Standard, 71),
            AnnualBudget::BelowEffectiveMinimum
        );
    }

    #[test]
    fn landmark_is_exempt_from_the_scarcity_ramp() {
        let candidate = LiveOpportunitySnapshot {
            committed_shows_year: 15,
            pipeline_shows_year: 3,
            ..snapshot()
        };
        // The same fill that raised the bar to 71 for a Standard show does not
        // touch a Landmark one at all.
        assert_eq!(
            annual_budget_verdict(
                candidate,
                LiveOpportunityPolicy::default(),
                StrategicTier::Landmark,
                1
            ),
            AnnualBudget::Clears
        );
    }

    #[test]
    fn a_full_year_never_drops_a_landmark_opportunity_it_escalates() {
        let candidate = LiveOpportunitySnapshot {
            committed_shows_year: 20,
            pipeline_shows_year: 0,
            strategic_value_basis_points: 9_000,
            fit_basis_points: 10_000,
            reputation_basis_points: 10_000,
            ..snapshot()
        };
        assert!(matches!(
            evaluate_live_opportunity(candidate, LiveOpportunityPolicy::default(), now()),
            LiveOpportunityDecision::EscalateLandmark { .. }
        ));
        // The identical calendar holds an ordinary show outright.
        let ordinary = LiveOpportunitySnapshot {
            strategic_value_basis_points: 0,
            ..candidate
        };
        assert_eq!(
            evaluate_live_opportunity(ordinary, LiveOpportunityPolicy::default(), now()),
            LiveOpportunityDecision::Hold
        );
    }

    #[test]
    fn the_bounded_loss_tolerance_applies_only_at_the_landmark_floor() {
        // A break-even-minus loss that a Landmark slot may run.
        let policy = LiveOpportunityPolicy {
            max_strategic_negative_margin_minor: 50_000,
            ..LiveOpportunityPolicy::default()
        };
        let losing_landmark = LiveOpportunitySnapshot {
            strategic_value_basis_points: 9_000,
            fit_basis_points: 10_000,
            reputation_basis_points: 10_000,
            expected_fee_minor: 80_000,
            estimated_cost_minor: 100_000,
            application_fee_minor: 0,
            ..snapshot()
        };
        // margin = -20_000, inside the 50_000 tolerance, but only for Landmark.
        assert!(may_auto_submit(
            losing_landmark,
            policy,
            StrategicTier::Landmark,
            live_opportunity_score(losing_landmark)
        ));
        assert!(!may_auto_submit(
            losing_landmark,
            policy,
            StrategicTier::Notable,
            live_opportunity_score(losing_landmark)
        ));
    }

    #[test]
    fn far_shot_requires_exceptional_score() {
        let mut candidate = snapshot();
        candidate.travel_band = Some(LiveTravelBand::FarShot);
        candidate.fit_basis_points = 7_500;
        candidate.reputation_basis_points = 7_500;
        assert_eq!(
            evaluate_live_opportunity(candidate, LiveOpportunityPolicy::default(), now()),
            LiveOpportunityDecision::Hold
        );
    }

    #[test]
    fn discovery_ignores_unrelated_public_text() {
        assert!(
            evaluate_live_opportunity_discovery(&LiveOpportunityDiscovery {
                title: "Newsletter",
                summary: "General music news",
            })
            .is_none()
        );
    }

    #[test]
    fn weak_or_expired_opportunity_is_ignored() {
        let mut candidate = snapshot();
        candidate.fit_basis_points = 1_000;
        candidate.deadline = Some(now() - Duration::hours(1));
        assert_eq!(
            evaluate_live_opportunity(candidate, LiveOpportunityPolicy::default(), now()),
            LiveOpportunityDecision::Hold
        );
    }

    #[test]
    fn an_uncosted_show_is_prepared_for_a_human_but_never_submitted_alone() {
        // The band does not commit to a long drive on a cost nobody computed.
        let uncosted = LiveOpportunitySnapshot {
            costed_from_logistics: false,
            ..snapshot()
        };
        assert!(matches!(
            evaluate_live_opportunity(uncosted, LiveOpportunityPolicy::default(), now()),
            LiveOpportunityDecision::PrepareForApproval { .. } | LiveOpportunityDecision::Hold
        ));
        assert!(!may_auto_submit(
            uncosted,
            LiveOpportunityPolicy::default(),
            StrategicTier::Standard,
            100
        ));
    }

    #[test]
    fn an_uncosted_show_scores_no_economics_points() {
        // Otherwise an unknown cost reads as break-even, and an uncosted gig
        // outranks a costed profitable one.
        let costed = snapshot();
        let uncosted = LiveOpportunitySnapshot {
            costed_from_logistics: false,
            ..costed
        };
        assert!(live_opportunity_score(uncosted) < live_opportunity_score(costed));
    }
}
