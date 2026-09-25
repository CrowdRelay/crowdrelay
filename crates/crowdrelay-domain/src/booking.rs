//! Booking-opportunity bounded context.
//!
//! First-party demand remains authoritative. Fresh external market evidence can
//! add only a bounded confirmation bonus; it can never create a venue, recipient
//! or commercial commitment by itself.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{
    BookingTargetId, CityId, VenueId, autonomy::Confidence, market_intelligence::CityMarketEvidence,
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
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_outreach_at: Option<OffsetDateTime>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct BookingOpportunityPolicy {
    pub minimum_score: u16,
    pub outreach_cooldown_days: u32,
    /// Venue/promoter discovery supply knobs. Serde-defaulted so stored
    /// policy rows written before discovery existed keep parsing.
    pub supply: crate::booking_discovery::BookingSupplyPolicy,
}

impl Default for BookingOpportunityPolicy {
    fn default() -> Self {
        Self {
            minimum_score: 65,
            outreach_cooldown_days: 30,
            supply: crate::booking_discovery::BookingSupplyPolicy::default(),
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

/// Read-side evidence about the room a booking target points at, collected
/// only when the target is venue-linked. `None` fields mean "not observed";
/// they are never fabricated as zeroes, and this row is attached to a target
/// only when a venue link exists — an unlinked target carries `None`, not an
/// empty row.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BookingVenueEvidence {
    /// Published/completed shows marked at the room during the last 365 days,
    /// across all tenants (`place_venue_marks` is shared graph data).
    pub shows_last_12m: i64,
    /// Distinct linked acts on room bills whose genres intersect the
    /// requesting tenant's genres, canonicalised through
    /// `place_genre_aliases`. The tenant's own acts are excluded.
    pub comparable_acts: i64,
    /// Resolved global `genres` fact for the room (`workspace_id IS NULL`,
    /// trust-ordered, unexpired). `None` when the graph holds none.
    pub genres: Option<String>,
    /// Resolved global `capacity` fact for the room. `None` when the graph
    /// holds none.
    pub capacity: Option<String>,
    /// Days since the newest past marked event at the room. `None` when the
    /// room has no observed past event.
    pub days_since_last_event: Option<i64>,
    /// Age in days of the requesting workspace's freshest private
    /// `booking_email` fact for this room. Freshness only — the email value
    /// itself never leaves the facts table.
    pub booking_contact_days: Option<i64>,
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
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_outreach_at: Option<OffsetDateTime>,
    pub followup_count: u16,
    pub last_reply: BookingReplyDisposition,
    /// Evidence for the linked room; `None` when the target has no venue link.
    pub venue_evidence: Option<BookingVenueEvidence>,
    /// Days until the festival's next application window closes (§12-5 entity
    /// 5). `None` for non-festival targets and festivals with no open window —
    /// both read as "nothing shutting", never as "infinite runway".
    pub days_until_application_close: Option<i64>,
    /// The close timestamp itself. The day count is the countdown a reader
    /// watches; the timestamp is the edition's identity — it changes only
    /// when a *different* edition becomes the next to close, which makes it
    /// the stable key a decision dedupes on.
    #[serde(with = "time::serde::rfc3339::option")]
    pub next_application_closes_at: Option<OffsetDateTime>,
    /// Every room this target resolves to: the primary `venue_id` union the
    /// promoter↔venue edges (§12-5 entity 6). Venue-level evidence should
    /// aggregate over this set — the unioned evidence read is the follow-up
    /// the 4V.7 snapshot extension owns; until then the ids are exposed so a
    /// reader can never see a promoter as room-less.
    pub linked_venue_ids: Vec<VenueId>,
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
#[serde(default)]
pub struct FestivalWindowPolicy {
    /// Propose when the next edition's application window closes within this
    /// many days — enough runway to draft, approve and send before it shuts.
    /// Longer than a follow-up cadence because a festival letter is a fresh
    /// ask, not a nudge on a live thread.
    pub ask_within_days: u32,
    /// The same contact cooldown the anchor selection enforces — a festival
    /// contacted inside the window is mid-conversation already.
    pub target_cooldown_days: u32,
}

impl Default for FestivalWindowPolicy {
    fn default() -> Self {
        Self {
            ask_within_days: 21,
            target_cooldown_days: 180,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FestivalWindowDecision {
    Hold(FestivalWindowHoldReason),
    Request { confidence: Confidence },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FestivalWindowHoldReason {
    NotAFestival,
    NoOpenWindow,
    WindowNotImminent,
    Inactive,
    NoBooking,
    OutreachInFlight,
    CooldownActive,
    /// A warm thread belongs to the operator working it, not to a fresh
    /// proposal — positive, booked and unclassified replies all live here.
    LiveThread,
    DoNotContact,
}

/// The deadline-driven ask: a festival's next edition closes applications
/// soon, and nobody has asked for this window yet.
///
/// Unlike the city-driven path this does not wait for fan demand to clear a
/// threshold — the festival slot is the demand. The guards it keeps are the
/// ones about contact, not merit: reachable, not mid-conversation, outside
/// cooldown, and not a thread a human is already working.
///
/// `declined` deliberately does not hold: festivals decline per edition, and
/// a new edition's window is a new ask the operator approves or not.
/// `do_not_contact` holds forever, and a warm reply (`received`, `positive`,
/// `booked`) holds because that thread is already alive.
#[must_use]
pub fn evaluate_festival_window(
    target: &BookingTargetSnapshot,
    policy: FestivalWindowPolicy,
    now: OffsetDateTime,
) -> FestivalWindowDecision {
    if target.kind != BookingTargetKind::Festival {
        return FestivalWindowDecision::Hold(FestivalWindowHoldReason::NotAFestival);
    }
    if !target.active {
        return FestivalWindowDecision::Hold(FestivalWindowHoldReason::Inactive);
    }
    if !target.accepts_booking {
        return FestivalWindowDecision::Hold(FestivalWindowHoldReason::NoBooking);
    }
    let Some(days_left) = target.days_until_application_close else {
        return FestivalWindowDecision::Hold(FestivalWindowHoldReason::NoOpenWindow);
    };
    if days_left > i64::from(policy.ask_within_days) {
        return FestivalWindowDecision::Hold(FestivalWindowHoldReason::WindowNotImminent);
    }
    if matches!(target.last_reply, BookingReplyDisposition::DoNotContact) {
        return FestivalWindowDecision::Hold(FestivalWindowHoldReason::DoNotContact);
    }
    if matches!(
        target.last_reply,
        BookingReplyDisposition::Received
            | BookingReplyDisposition::Positive
            | BookingReplyDisposition::Booked
    ) {
        return FestivalWindowDecision::Hold(FestivalWindowHoldReason::LiveThread);
    }
    if target.outreach_in_flight {
        return FestivalWindowDecision::Hold(FestivalWindowHoldReason::OutreachInFlight);
    }
    if target.last_outreach_at.is_some_and(|at| {
        at > now || now - at < Duration::days(i64::from(policy.target_cooldown_days))
    }) {
        return FestivalWindowDecision::Hold(FestivalWindowHoldReason::CooldownActive);
    }
    let relationship_bonus = target.relationship_score.saturating_mul(20).min(2_000);
    FestivalWindowDecision::Request {
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

/// The deterministic score `select_booking_target` ranks targets by, or `None`
/// when the target is ineligible. Priority is the operator-owned commercial
/// intent and must remain the dominant selector. Relationship quality refines
/// it, while capacity fit is deliberately bounded so a "perfect room" can't
/// override a materially stronger trusted relationship.
///
/// `enforce_cooldown` is `true` when picking the anchor — a target inside its
/// cooldown window must not be contacted. Additional recipients on an already
/// approved outreach pass `false`: the email is going out either way, and a
/// room the tenant last wrote to six months ago is a perfectly good second
/// recipient for the same booking run.
#[must_use]
fn target_selection_score(
    target: &BookingTargetSnapshot,
    city_id: CityId,
    expected_attendance: u32,
    policy: &BookingTargetSelectionPolicy,
    now: OffsetDateTime,
    enforce_cooldown: bool,
) -> Option<u16> {
    let cooldown = Duration::days(i64::from(policy.target_cooldown_days));
    if target.city_id != city_id
        || target.version <= 0
        || !target.active
        || !target.accepts_booking
        || target.priority > 100
        || target.relationship_score > 100
        || target.priority < policy.minimum_priority
        || target.outreach_in_flight
        || target.last_outreach_at.is_some_and(|at| at > now)
        || (enforce_cooldown
            && target
                .last_outreach_at
                .is_some_and(|at| now - at < cooldown))
    {
        return None;
    }
    let capacity_fit = capacity_fit_score(target.capacity, expected_attendance);
    Some(
        target
            .priority
            .saturating_mul(60)
            .saturating_add(target.relationship_score.saturating_mul(25))
            .saturating_add(capacity_fit.saturating_mul(15))
            / 100,
    )
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
    let mut best: Option<(&BookingTargetSnapshot, u16)> = None;
    for target in targets {
        let Some(score) =
            target_selection_score(target, city_id, expected_attendance, &policy, now, true)
        else {
            continue;
        };
        let replace = best.is_none_or(|(current, current_score)| {
            score > current_score
                || (score == current_score
                    && (target.priority > current.priority
                        || (target.priority == current.priority
                            && (target.last_outreach_at < current.last_outreach_at
                                || (target.last_outreach_at == current.last_outreach_at
                                    && target.target_id < current.target_id)))))
        });
        if replace {
            best = Some((target, score));
        }
    }

    best.map_or(
        BookingTargetDecision::NoEligibleTarget,
        |(target, score)| BookingTargetDecision::Selected {
            target_id: target.target_id,
            target_version: target.version,
            selection_score: score,
        },
    )
}

/// Maximum number of additional recipients a booking outreach may carry on
/// top of its anchor. Hard bound so a crowded city can never turn one
/// approval into an unbounded mail-merge.
pub const MAX_ADDITIONAL_BOOKING_RECIPIENTS: usize = 4;

/// Picks extra same-city recipients for an already-approved booking outreach.
/// The anchor is excluded; survivors are the next best targets under the same
/// deterministic score the anchor selection uses, ordered by score then the
/// same priority/recency/id tie-break, capped at `limit`.
///
/// The cooldown gate is deliberately relaxed here: the outreach exists
/// because the anchor passed the full gate, and copying a room the tenant
/// already wrote to once is bounded extra reach, not a new touch pattern.
/// `outreach_in_flight` still excludes a target mid-conversation.
#[must_use]
pub fn additional_booking_recipients(
    city_id: CityId,
    expected_attendance: u32,
    targets: &[BookingTargetSnapshot],
    anchor_id: BookingTargetId,
    policy: &BookingTargetSelectionPolicy,
    now: OffsetDateTime,
    limit: usize,
) -> Vec<(BookingTargetId, i64)> {
    let mut ranked: Vec<(&BookingTargetSnapshot, u16)> = targets
        .iter()
        .filter(|target| target.target_id != anchor_id)
        .filter_map(|target| {
            target_selection_score(target, city_id, expected_attendance, policy, now, false)
                .map(|score| (target, score))
        })
        .collect();
    ranked.sort_by(|(a, a_score), (b, b_score)| {
        b_score
            .cmp(a_score)
            .then_with(|| b.priority.cmp(&a.priority))
            // Older outreach first — None (never contacted) wins over any
            // timestamp, matching the anchor tie-break.
            .then_with(|| a.last_outreach_at.cmp(&b.last_outreach_at))
            .then_with(|| a.target_id.cmp(&b.target_id))
    });
    ranked
        .into_iter()
        .take(limit.min(MAX_ADDITIONAL_BOOKING_RECIPIENTS))
        .map(|(target, _)| (target.target_id, target.version))
        .collect()
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
            venue_evidence: None,
            days_until_application_close: None,
            next_application_closes_at: None,
            linked_venue_ids: Vec::new(),
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
    fn target_selection_tie_break_is_stable_without_sorting_or_allocation() {
        let city = CityId::new();
        let mut first = target(city, 80, 80);
        let mut second = target(city, 80, 80);
        first.last_outreach_at = None;
        second.last_outreach_at = None;
        let expected = if first.target_id < second.target_id {
            first.target_id
        } else {
            second.target_id
        };

        let forward = [first.clone(), second.clone()];
        let reverse = [second, first];
        for candidates in [&forward[..], &reverse[..]] {
            assert!(matches!(
                select_booking_target(
                    city,
                    100,
                    candidates,
                    BookingTargetSelectionPolicy::default(),
                    now()
                ),
                BookingTargetDecision::Selected { target_id, .. } if target_id == expected
            ));
        }
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
            venue_evidence: None,
            days_until_application_close: None,
            next_application_closes_at: None,
            linked_venue_ids: Vec::new(),
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
    fn additional_recipients_skip_the_anchor_and_stay_deterministic() {
        let city = CityId::new();
        let anchor = target(city, 100, 100);
        let mid = target(city, 80, 80);
        let weak = target(city, 50, 50);
        let other_city = target(CityId::new(), 100, 100);
        let picked = additional_booking_recipients(
            city,
            100,
            &[
                weak.clone(),
                anchor.clone(),
                mid.clone(),
                other_city.clone(),
            ],
            anchor.target_id,
            &BookingTargetSelectionPolicy::default(),
            now(),
            4,
        );
        assert_eq!(picked, vec![(mid.target_id, 1), (weak.target_id, 1)]);
        // Order in the input must not matter: the same set reversed picks the
        // same recipients in the same order.
        let reversed = additional_booking_recipients(
            city,
            100,
            &[other_city, mid.clone(), anchor.clone(), weak.clone()],
            anchor.target_id,
            &BookingTargetSelectionPolicy::default(),
            now(),
            4,
        );
        assert_eq!(reversed, vec![(mid.target_id, 1), (weak.target_id, 1)]);
    }

    #[test]
    fn additional_recipients_never_exceed_the_cap_and_keep_flying_targets_out() {
        let city = CityId::new();
        let anchor = target(city, 100, 100);
        let mut inflight = target(city, 90, 90);
        inflight.outreach_in_flight = true;
        let others: Vec<BookingTargetSnapshot> = (0..6).map(|_| target(city, 70, 70)).collect();
        let mut pool = vec![anchor.clone(), inflight];
        pool.extend(others.iter().cloned());
        let picked = additional_booking_recipients(
            city,
            100,
            &pool,
            anchor.target_id,
            &BookingTargetSelectionPolicy::default(),
            now(),
            6,
        );
        // Six candidates, a cap of four, one in flight: at most four real
        // picks and the in-flight target is never among them.
        assert_eq!(picked.len(), 4);
        assert!(picked.iter().all(|(id, _)| *id != anchor.target_id));
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

    fn festival_target(city: CityId, days_left: i64) -> BookingTargetSnapshot {
        let mut festival = target(city, 80, 50);
        festival.kind = BookingTargetKind::Festival;
        festival.display_name = "Brutal Assault".to_owned();
        festival.days_until_application_close = Some(days_left);
        festival.next_application_closes_at = Some(now() + Duration::days(days_left));
        festival
    }

    #[test]
    fn a_closing_festival_window_proposes_the_ask() {
        let festival = festival_target(CityId::new(), 14);
        assert!(matches!(
            evaluate_festival_window(&festival, FestivalWindowPolicy::default(), now()),
            FestivalWindowDecision::Request { .. }
        ));
    }

    #[test]
    fn a_distant_festival_window_waits() {
        let festival = festival_target(CityId::new(), 60);
        assert_eq!(
            evaluate_festival_window(&festival, FestivalWindowPolicy::default(), now()),
            FestivalWindowDecision::Hold(FestivalWindowHoldReason::WindowNotImminent)
        );
    }

    #[test]
    fn a_venue_with_a_window_is_not_a_festival_ask() {
        let mut venue = festival_target(CityId::new(), 5);
        venue.kind = BookingTargetKind::Venue;
        assert_eq!(
            evaluate_festival_window(&venue, FestivalWindowPolicy::default(), now()),
            FestivalWindowDecision::Hold(FestivalWindowHoldReason::NotAFestival)
        );
    }

    #[test]
    fn a_festival_with_no_window_never_proposes() {
        let mut festival = festival_target(CityId::new(), 0);
        festival.days_until_application_close = None;
        festival.next_application_closes_at = None;
        assert_eq!(
            evaluate_festival_window(&festival, FestivalWindowPolicy::default(), now()),
            FestivalWindowDecision::Hold(FestivalWindowHoldReason::NoOpenWindow)
        );
    }

    #[test]
    fn a_closing_window_never_reopens_a_do_not_contact() {
        let mut festival = festival_target(CityId::new(), 3);
        festival.last_reply = BookingReplyDisposition::DoNotContact;
        assert_eq!(
            evaluate_festival_window(&festival, FestivalWindowPolicy::default(), now()),
            FestivalWindowDecision::Hold(FestivalWindowHoldReason::DoNotContact)
        );
    }

    #[test]
    fn a_warm_thread_is_left_to_the_operator() {
        for disposition in [
            BookingReplyDisposition::Received,
            BookingReplyDisposition::Positive,
            BookingReplyDisposition::Booked,
        ] {
            let mut festival = festival_target(CityId::new(), 3);
            festival.last_reply = disposition;
            assert_eq!(
                evaluate_festival_window(&festival, FestivalWindowPolicy::default(), now()),
                FestivalWindowDecision::Hold(FestivalWindowHoldReason::LiveThread),
                "{disposition:?} must hold"
            );
        }
    }

    #[test]
    fn a_decline_on_the_last_edition_does_not_block_the_next() {
        let mut festival = festival_target(CityId::new(), 10);
        festival.last_reply = BookingReplyDisposition::Declined;
        assert!(matches!(
            evaluate_festival_window(&festival, FestivalWindowPolicy::default(), now()),
            FestivalWindowDecision::Request { .. }
        ));
    }

    #[test]
    fn a_recent_letter_is_inside_the_cooldown() {
        let mut festival = festival_target(CityId::new(), 10);
        festival.last_outreach_at = Some(now() - Duration::days(30));
        assert_eq!(
            evaluate_festival_window(&festival, FestivalWindowPolicy::default(), now()),
            FestivalWindowDecision::Hold(FestivalWindowHoldReason::CooldownActive)
        );
    }

    #[test]
    fn an_in_flight_letter_holds_the_deadline_ask() {
        let mut festival = festival_target(CityId::new(), 10);
        festival.outreach_in_flight = true;
        assert_eq!(
            evaluate_festival_window(&festival, FestivalWindowPolicy::default(), now()),
            FestivalWindowDecision::Hold(FestivalWindowHoldReason::OutreachInFlight)
        );
    }
}
