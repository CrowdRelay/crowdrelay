//! Booking-opportunity bounded context.
//!
//! First-party demand remains authoritative. Fresh external market evidence can
//! add only a bounded confirmation bonus; it can never create a venue, recipient
//! or commercial commitment by itself.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{
    BookingTargetId, CityId, autonomy::Confidence, market_intelligence::CityMarketEvidence,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CityOpportunitySnapshot {
    pub city_id: CityId,
    pub active_fans: u32,
    pub new_fans_30d: u32,
    pub event_interests: u32,
    pub area_claims: u32,
    pub months_since_last_show: Option<u32>,
    pub market_evidence: Option<CityMarketEvidence>,
    pub outreach_in_flight: bool,
    pub last_outreach_at: Option<OffsetDateTime>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BookingOpportunityPolicy {
    pub minimum_score: u16,
    pub outreach_cooldown_days: u32,
}

impl Default for BookingOpportunityPolicy {
    fn default() -> Self {
        Self {
            minimum_score: 65,
            outreach_cooldown_days: 30,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BookingOpportunityDecision {
    Hold(BookingOpportunityHoldReason),
    RequestOutreach { score: u16, confidence: Confidence },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BookingOpportunityHoldReason {
    InvalidPolicy,
    InvalidSnapshot,
    InsufficientDemand,
    OutreachAlreadyInFlight,
    CooldownActive,
}

/// Calculates a stable `0..=100` opportunity score from first-party signals.
/// Weights are intentionally visible and testable.
#[must_use]
pub fn opportunity_score(snapshot: CityOpportunitySnapshot) -> u16 {
    let fan_points = snapshot.active_fans.min(100) * 30 / 100;
    let growth_points = snapshot.new_fans_30d.min(25) * 20 / 25;
    let interest_points = snapshot.event_interests.min(50) * 20 / 50;
    let area_points = snapshot.area_claims.min(20) * 10 / 20;
    let recency_points = snapshot
        .months_since_last_show
        .map_or(20, |months| months.min(12) * 20 / 12);
    let market_points = snapshot.market_evidence.map_or(0_u32, |evidence| {
        let confirmed_score = u64::from(evidence.score_basis_points)
            .saturating_mul(u64::from(evidence.confidence.basis_points()))
            / 10_000;
        u32::try_from(confirmed_score.saturating_mul(10) / 10_000)
            .unwrap_or(10)
            .min(10)
    });
    let total = fan_points
        .saturating_add(growth_points)
        .saturating_add(interest_points)
        .saturating_add(area_points)
        .saturating_add(recency_points)
        .saturating_add(market_points)
        .min(100);
    total as u16
}

#[must_use]
pub fn evaluate_booking_opportunity(
    snapshot: CityOpportunitySnapshot,
    policy: BookingOpportunityPolicy,
    now: OffsetDateTime,
) -> BookingOpportunityDecision {
    if policy.minimum_score > 100 {
        return BookingOpportunityDecision::Hold(BookingOpportunityHoldReason::InvalidPolicy);
    }
    if snapshot.last_outreach_at.is_some_and(|at| at > now) {
        return BookingOpportunityDecision::Hold(BookingOpportunityHoldReason::InvalidSnapshot);
    }
    if snapshot.outreach_in_flight {
        return BookingOpportunityDecision::Hold(
            BookingOpportunityHoldReason::OutreachAlreadyInFlight,
        );
    }
    if snapshot.last_outreach_at.is_some_and(|last_outreach| {
        now - last_outreach < Duration::days(i64::from(policy.outreach_cooldown_days))
    }) {
        return BookingOpportunityDecision::Hold(BookingOpportunityHoldReason::CooldownActive);
    }

    let score = opportunity_score(snapshot);
    if score < policy.minimum_score {
        return BookingOpportunityDecision::Hold(BookingOpportunityHoldReason::InsufficientDemand);
    }

    let score_bonus = score
        .saturating_sub(policy.minimum_score)
        .saturating_mul(100)
        .min(3_000);
    let market_confidence_bonus = snapshot.market_evidence.map_or(0_u16, |evidence| {
        let confirmed_score = u32::from(evidence.score_basis_points)
            .saturating_mul(u32::from(evidence.confidence.basis_points()))
            / 10_000;
        u16::try_from(confirmed_score / 10)
            .unwrap_or(1_000)
            .min(1_000)
    });
    let confidence = Confidence::saturating_from_basis_points(
        7_000_u16
            .saturating_add(score_bonus)
            .saturating_add(market_confidence_bonus),
    );
    BookingOpportunityDecision::RequestOutreach { score, confidence }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BookingTargetKind {
    Venue,
    Promoter,
    Festival,
}

impl BookingTargetKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Venue => "venue",
            Self::Promoter => "promoter",
            Self::Festival => "festival",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BookingReplyDisposition {
    None,
    /// A reply exists, but no semantic classification was required.
    Received,
    Positive,
    Declined,
    Booked,
    DoNotContact,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BookingOutreachPhase {
    Initial,
    FollowUp,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BookingFollowUpPolicy {
    pub followup_after_days: u32,
    pub maximum_followups: u16,
}

impl Default for BookingFollowUpPolicy {
    fn default() -> Self {
        Self {
            followup_after_days: 5,
            maximum_followups: 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BookingFollowUpDecision {
    Hold,
    Request { confidence: Confidence },
}

/// Operator-verified commercial contact available to the Booking bounded context.
/// Contact details themselves stay in infrastructure; the domain only receives
/// facts required to select a target safely and deterministically.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BookingTargetSnapshot {
    pub target_id: BookingTargetId,
    pub city_id: CityId,
    pub kind: BookingTargetKind,
    pub display_name: String,
    /// Optional verified room/event capacity. `None` is neutral for selection.
    pub capacity: Option<u32>,
    pub version: i64,
    pub active: bool,
    pub accepts_booking: bool,
    /// Explicit operator priority, `0..=100`.
    pub priority: u16,
    /// Relationship quality derived from verified outcomes, `0..=100`.
    pub relationship_score: u16,
    pub outreach_in_flight: bool,
    pub last_outreach_at: Option<OffsetDateTime>,
    pub followup_count: u16,
    pub last_reply: BookingReplyDisposition,
}

#[must_use]
pub fn evaluate_booking_followup(
    target: &BookingTargetSnapshot,
    policy: BookingFollowUpPolicy,
    now: OffsetDateTime,
) -> BookingFollowUpDecision {
    if policy.followup_after_days == 0
        || policy.maximum_followups == 0
        || target.version <= 0
        || !target.active
        || !target.accepts_booking
        || target.outreach_in_flight
        || target.followup_count >= policy.maximum_followups
        || !matches!(target.last_reply, BookingReplyDisposition::None)
    {
        return BookingFollowUpDecision::Hold;
    }
    let Some(last_outreach) = target.last_outreach_at else {
        return BookingFollowUpDecision::Hold;
    };
    if last_outreach > now
        || now - last_outreach < Duration::days(i64::from(policy.followup_after_days))
    {
        return BookingFollowUpDecision::Hold;
    }
    let relationship_bonus = target.relationship_score.saturating_mul(20).min(2_000);
    BookingFollowUpDecision::Request {
        confidence: Confidence::saturating_from_basis_points(
            7_500_u16.saturating_add(relationship_bonus),
        ),
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BookingTargetSelectionPolicy {
    pub minimum_priority: u16,
    pub target_cooldown_days: u32,
    pub algorithm_version: u16,
}

impl Default for BookingTargetSelectionPolicy {
    fn default() -> Self {
        Self {
            minimum_priority: 20,
            target_cooldown_days: 180,
            algorithm_version: 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BookingTargetDecision {
    NoEligibleTarget,
    Selected {
        target_id: BookingTargetId,
        target_version: i64,
        selection_score: u16,
    },
}

/// Conservative first-party headcount estimate used only for venue-size fit.
/// It is deliberately capped and does not use external market signals.
#[must_use]
pub fn estimated_attendance(snapshot: CityOpportunitySnapshot) -> u32 {
    let interests = snapshot.event_interests.min(250);
    let existing_fans = snapshot.active_fans.min(500).saturating_mul(20) / 100;
    let fresh_fans = snapshot.new_fans_30d.min(100).saturating_mul(10) / 100;
    let area = snapshot.area_claims.min(100).saturating_mul(25) / 100;
    interests
        .saturating_add(existing_fans)
        .saturating_add(fresh_fans)
        .saturating_add(area)
        .clamp(20, 500)
}

const fn capacity_fit_score(capacity: Option<u32>, expected_attendance: u32) -> u16 {
    let Some(capacity) = capacity else {
        return 50;
    };
    if capacity == 0 || expected_attendance == 0 {
        return 0;
    }
    let expected = expected_attendance as u64;
    let capacity = capacity as u64;
    if capacity >= expected && capacity <= expected.saturating_mul(2) {
        100
    } else if capacity.saturating_mul(10) >= expected.saturating_mul(7)
        && capacity <= expected.saturating_mul(3)
    {
        70
    } else if capacity.saturating_mul(2) < expected {
        10
    } else {
        30
    }
}

/// Chooses one verified target. Stable ordering makes the decision reproducible:
/// operator-owned priority dominates, relationship quality refines the choice,
/// verified capacity fit contributes a bounded bonus, and the typed UUID provides
/// the final tie-breaker.
#[must_use]
pub fn select_booking_target(
    city_id: CityId,
    expected_attendance: u32,
    targets: &[BookingTargetSnapshot],
    policy: BookingTargetSelectionPolicy,
    now: OffsetDateTime,
) -> BookingTargetDecision {
    if policy.minimum_priority > 100 || policy.algorithm_version == 0 {
        return BookingTargetDecision::NoEligibleTarget;
    }
    let cooldown = Duration::days(i64::from(policy.target_cooldown_days));
    let mut eligible = targets
        .iter()
        .filter(|target| {
            target.city_id == city_id
                && target.version > 0
                && target.active
                && target.accepts_booking
                && target.priority <= 100
                && target.relationship_score <= 100
                && target.priority >= policy.minimum_priority
                && !target.outreach_in_flight
                && !target.last_outreach_at.is_some_and(|at| at > now)
                && !target
                    .last_outreach_at
                    .is_some_and(|at| now - at < cooldown)
        })
        .map(|target| {
            let capacity_fit = capacity_fit_score(target.capacity, expected_attendance);
            // Priority is the operator-owned commercial intent and must remain
            // the dominant selector. Relationship quality refines it, while
            // capacity fit is deliberately bounded so a "perfect room" can't
            // override a materially stronger trusted relationship.
            let score = target
                .priority
                .saturating_mul(60)
                .saturating_add(target.relationship_score.saturating_mul(25))
                .saturating_add(capacity_fit.saturating_mul(15))
                / 100;
            (target, score)
        })
        .collect::<Vec<_>>();
    eligible.sort_by(|(left, left_score), (right, right_score)| {
        right_score
            .cmp(left_score)
            .then_with(|| right.priority.cmp(&left.priority))
            .then_with(|| left.last_outreach_at.cmp(&right.last_outreach_at))
            .then_with(|| left.target_id.cmp(&right.target_id))
    });
    eligible.first().map_or(
        BookingTargetDecision::NoEligibleTarget,
        |(target, score)| BookingTargetDecision::Selected {
            target_id: target.target_id,
            target_version: target.version,
            selection_score: *score,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    fn strong_city() -> CityOpportunitySnapshot {
        CityOpportunitySnapshot {
            city_id: CityId::new(),
            active_fans: 90,
            new_fans_30d: 20,
            event_interests: 40,
            area_claims: 10,
            months_since_last_show: Some(12),
            market_evidence: None,
            outreach_in_flight: false,
            last_outreach_at: None,
        }
    }

    #[test]
    fn strong_first_party_demand_produces_outreach_intent() {
        assert!(matches!(
            evaluate_booking_opportunity(strong_city(), BookingOpportunityPolicy::default(), now()),
            BookingOpportunityDecision::RequestOutreach { .. }
        ));
    }

    #[test]
    fn inflight_outreach_prevents_duplicate_contact() {
        let mut snapshot = strong_city();
        snapshot.outreach_in_flight = true;
        assert_eq!(
            evaluate_booking_opportunity(snapshot, BookingOpportunityPolicy::default(), now()),
            BookingOpportunityDecision::Hold(BookingOpportunityHoldReason::OutreachAlreadyInFlight)
        );
    }

    #[test]
    fn recent_outreach_enforces_domain_cooldown() {
        let mut snapshot = strong_city();
        snapshot.last_outreach_at = Some(now() - Duration::days(10));
        assert_eq!(
            evaluate_booking_opportunity(snapshot, BookingOpportunityPolicy::default(), now()),
            BookingOpportunityDecision::Hold(BookingOpportunityHoldReason::CooldownActive)
        );
    }

    #[test]
    fn score_is_bounded_even_for_extreme_counts() {
        let mut snapshot = strong_city();
        snapshot.active_fans = u32::MAX;
        snapshot.new_fans_30d = u32::MAX;
        snapshot.event_interests = u32::MAX;
        snapshot.area_claims = u32::MAX;
        assert_eq!(opportunity_score(snapshot), 100);
    }
    #[test]
    fn external_market_evidence_is_only_a_bounded_confirmation_bonus() {
        let mut snapshot = strong_city();
        snapshot.active_fans = 0;
        snapshot.new_fans_30d = 0;
        snapshot.event_interests = 0;
        snapshot.area_claims = 0;
        snapshot.months_since_last_show = Some(0);
        snapshot.market_evidence = Some(CityMarketEvidence {
            score_basis_points: 10_000,
            confidence: Confidence::saturating_from_basis_points(10_000),
            signal_families: 4,
        });
        assert_eq!(opportunity_score(snapshot), 10);
        assert_eq!(
            evaluate_booking_opportunity(snapshot, BookingOpportunityPolicy::default(), now()),
            BookingOpportunityDecision::Hold(BookingOpportunityHoldReason::InsufficientDemand),
        );
    }

    fn target(city_id: CityId, priority: u16, relationship_score: u16) -> BookingTargetSnapshot {
        BookingTargetSnapshot {
            target_id: BookingTargetId::new(),
            city_id,
            kind: BookingTargetKind::Venue,
            display_name: "Example Venue".to_owned(),
            capacity: None,
            version: 1,
            active: true,
            accepts_booking: true,
            priority,
            relationship_score,
            outreach_in_flight: false,
            last_outreach_at: None,
            followup_count: 0,
            last_reply: BookingReplyDisposition::None,
        }
    }

    #[test]
    fn target_selection_is_deterministic_and_prefers_verified_priority() {
        let city = CityId::new();
        let lower = target(city, 60, 100);
        let higher = target(city, 90, 40);
        let higher_id = higher.target_id;
        assert_eq!(
            select_booking_target(
                city,
                100,
                &[lower, higher],
                BookingTargetSelectionPolicy::default(),
                now()
            ),
            BookingTargetDecision::Selected {
                target_id: higher_id,
                target_version: 1,
                selection_score: 71,
            }
        );
    }

    #[test]
    fn target_cooldown_and_inflight_prevent_contact_spam() {
        let city = CityId::new();
        let mut recent = target(city, 100, 100);
        recent.last_outreach_at = Some(now() - Duration::days(30));
        let mut inflight = target(city, 100, 100);
        inflight.outreach_in_flight = true;
        assert_eq!(
            select_booking_target(
                city,
                100,
                &[recent, inflight],
                BookingTargetSelectionPolicy::default(),
                now()
            ),
            BookingTargetDecision::NoEligibleTarget
        );
    }

    #[test]
    fn target_from_another_city_is_never_selected() {
        let city = CityId::new();
        let other = target(CityId::new(), 100, 100);
        assert_eq!(
            select_booking_target(
                city,
                100,
                &[other],
                BookingTargetSelectionPolicy::default(),
                now()
            ),
            BookingTargetDecision::NoEligibleTarget
        );
    }

    #[test]
    fn unanswered_initial_booking_touch_gets_one_bounded_followup() {
        let target = BookingTargetSnapshot {
            target_id: BookingTargetId::new(),
            city_id: CityId::new(),
            kind: BookingTargetKind::Venue,
            display_name: "Venue".to_owned(),
            capacity: Some(120),
            version: 1,
            active: true,
            accepts_booking: true,
            priority: 70,
            relationship_score: 70,
            outreach_in_flight: false,
            last_outreach_at: Some(now() - Duration::days(6)),
            followup_count: 0,
            last_reply: BookingReplyDisposition::None,
        };
        assert!(matches!(
            evaluate_booking_followup(&target, BookingFollowUpPolicy::default(), now()),
            BookingFollowUpDecision::Request { .. }
        ));
        let replied = BookingTargetSnapshot {
            last_reply: BookingReplyDisposition::Received,
            ..target.clone()
        };
        assert_eq!(
            evaluate_booking_followup(&replied, BookingFollowUpPolicy::default(), now()),
            BookingFollowUpDecision::Hold,
        );
    }

    #[test]
    fn attendance_estimate_uses_only_first_party_facts() {
        let mut city = strong_city();
        city.market_evidence = None;
        assert_eq!(estimated_attendance(city), 62);
    }

    #[test]
    fn capacity_fit_breaks_equal_relationship_ties() {
        let city = CityId::new();
        let mut oversized = target(city, 80, 80);
        oversized.display_name = "Oversized".to_owned();
        oversized.capacity = Some(1_000);
        let mut fitted = target(city, 80, 80);
        fitted.display_name = "Fitted".to_owned();
        fitted.capacity = Some(120);
        assert!(matches!(
            select_booking_target(
                city,
                100,
                &[oversized, fitted.clone()],
                BookingTargetSelectionPolicy::default(),
                now(),
            ),
            BookingTargetDecision::Selected { target_id, .. } if target_id == fitted.target_id
        ));
    }
}
