//! Growth debt: work the business already committed to and then left undone.
//!
//! The `growth_metrics` context watches numbers move. This module watches the
//! opposite failure, which no trend can see: a warm relationship nobody
//! answered, a show whose free distribution levers were never pulled, a release
//! whose plan stopped halfway, a contact record nobody has confirmed in months.
//! None of that shows up as an anomaly, because nothing happened at all.
//!
//! Every input is a first-party row that already exists. This module adds no
//! storage: it normalizes "how long has this been outstanding" into one shape
//! and decides whether the neglect is now worth an operator's attention.
//!
//! Two refusals are deliberate. Debt whose deadline has already passed is never
//! raised — a show that already played cannot be promoted, and reporting it
//! would spend attention on something no action can change. And debt is never
//! claimed from an empty denominator: if nothing was ever tracked for a
//! subject, "none of it was done" is a statement about our records, not about
//! the business.
//!
//! Integer arithmetic only; ratios in basis points, matching the rest of the
//! domain.

use serde::{Deserialize, Serialize};

use crate::{
    BeaconId, BookingTargetId, EventId, OutreachTargetId, ReleasePlanId, autonomy::Confidence,
    growth_metrics::MetricValueTier,
};

/// The kind of neglect observed. Each kind carries its own horizon, its own
/// distance from banked value, and its own remedy vocabulary.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GrowthDebtKind {
    /// A relationship with a positive score that nobody has touched since its
    /// horizon. Warmth decays; this is the cheapest growth there is to lose.
    RelationshipQuiet,
    /// An upcoming event whose declared show-growth surfaces are still
    /// unrequested past their lead time. The aggregate of what was skipped, not
    /// a second copy of the `show_growth` lever rule.
    EventLeversSkipped,
    /// An active release plan whose milestones stopped being recorded while the
    /// release date kept approaching.
    ReleaseMilestonesMissed,
    /// Contact data nobody has re-verified inside the policy horizon. Hygiene:
    /// no outcome is at stake yet, but every other debt rule depends on it.
    ///
    /// No adapter can supply this yet, and that is deliberate. The schema has
    /// no verification timestamp anywhere — `verified` is a boolean and
    /// `updated_at` moves whenever any column does, so reading it as "last
    /// confirmed" would claim evidence that does not exist. The rule is here
    /// and tested; wiring it needs a real clock first.
    StaleContactData,
}

impl GrowthDebtKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RelationshipQuiet => "relationship_quiet",
            Self::EventLeversSkipped => "event_levers_skipped",
            Self::ReleaseMilestonesMissed => "release_milestones_missed",
            Self::StaleContactData => "stale_contact_data",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "relationship_quiet" => Some(Self::RelationshipQuiet),
            "event_levers_skipped" => Some(Self::EventLeversSkipped),
            "release_milestones_missed" => Some(Self::ReleaseMilestonesMissed),
            "stale_contact_data" => Some(Self::StaleContactData),
            _ => None,
        }
    }

    /// What the evidence says. Describes the record, never a cause: the domain
    /// does not know *why* the work was skipped and must not guess.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::RelationshipQuiet => {
                "a warm relationship has had no interaction in either direction past its horizon"
            }
            Self::EventLeversSkipped => {
                "an upcoming event has declared growth surfaces still unrequested past their lead time"
            }
            Self::ReleaseMilestonesMissed => {
                "an active release plan has milestones still unrecorded past their grace period"
            }
            Self::StaleContactData => {
                "a contact record has not been re-verified inside the policy horizon"
            }
        }
    }

    /// The class of remedy. Concrete playbooks are a template concern; the
    /// domain refuses to assume a channel exists that it was never told about.
    #[must_use]
    pub const fn recommended_action(self) -> &'static str {
        match self {
            Self::RelationshipQuiet => "revive_quiet_relationship",
            Self::EventLeversSkipped => "complete_event_growth_surfaces",
            Self::ReleaseMilestonesMissed => "resume_release_plan",
            Self::StaleContactData => "reverify_contact_data",
        }
    }

    /// The decision kind recorded against this finding.
    ///
    /// Per-kind rather than one shared value, because the cooldown is read back
    /// out of `viryaos_autopilot_decisions` by grouping on it: one event can owe
    /// both skipped levers and a stalled release plan, and raising one must not
    /// silence the other for a fortnight.
    #[must_use]
    pub const fn decision_kind(self) -> &'static str {
        match self {
            Self::RelationshipQuiet => "raise_growth_debt_relationship_quiet",
            Self::EventLeversSkipped => "raise_growth_debt_event_levers_skipped",
            Self::ReleaseMilestonesMissed => "raise_growth_debt_release_milestones_missed",
            Self::StaleContactData => "raise_growth_debt_stale_contact_data",
        }
    }

    #[must_use]
    pub const fn template_key(self) -> &'static str {
        match self {
            Self::RelationshipQuiet => "growth_debt_relationship_quiet",
            Self::EventLeversSkipped => "growth_debt_event_levers",
            Self::ReleaseMilestonesMissed => "growth_debt_release_milestones",
            Self::StaleContactData => "growth_debt_contact_data",
        }
    }

    /// How close the neglected work sits to value the business banks. Shared
    /// vocabulary with `growth_metrics` on purpose: one ordering decides what
    /// outranks what, so relationship hygiene can never outrank a show whose
    /// distribution never went out.
    #[must_use]
    pub const fn value_tier(self) -> MetricValueTier {
        match self {
            Self::EventLeversSkipped | Self::ReleaseMilestonesMissed => MetricValueTier::Downstream,
            Self::RelationshipQuiet => MetricValueTier::Intermediate,
            Self::StaleContactData => MetricValueTier::Vanity,
        }
    }

    /// True when the subject has a date that makes the debt expire. Those kinds
    /// are dropped once the date passes rather than reported forever.
    #[must_use]
    pub const fn is_deadline_bound(self) -> bool {
        matches!(
            self,
            Self::EventLeversSkipped | Self::ReleaseMilestonesMissed
        )
    }

    /// True when the rule needs a relationship score before it may speak. A
    /// cold contact going quiet is not debt, it is a contact that was never
    /// warm; without the score there is no evidence either way.
    #[must_use]
    pub const fn requires_relationship_score(self) -> bool {
        matches!(self, Self::RelationshipQuiet)
    }

    const fn base_priority(self) -> u16 {
        match self {
            Self::EventLeversSkipped => 75,
            Self::ReleaseMilestonesMissed => 65,
            Self::RelationshipQuiet => 50,
            Self::StaleContactData => 30,
        }
    }
}

/// The first-party row the debt is attached to. Kept typed so the application
/// layer maps it to an `ActionSubject` without re-parsing a string.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "id")]
pub enum GrowthDebtSubject {
    BookingTarget(BookingTargetId),
    OutreachTarget(OutreachTargetId),
    Beacon(BeaconId),
    Event(EventId),
    ReleasePlan(ReleasePlanId),
}

impl GrowthDebtSubject {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BookingTarget(_) => "booking_target",
            Self::OutreachTarget(_) => "outreach_target",
            Self::Beacon(_) => "beacon",
            Self::Event(_) => "event",
            Self::ReleasePlan(_) => "release_plan",
        }
    }
}

/// Everything the rule needs about one neglected subject at evaluation time.
///
/// The adapter supplies facts only: how long the work has been outstanding, how
/// much of it is outstanding, and what dates apply. Every horizon and every
/// threshold lives in [`GrowthDebtPolicy`], so changing what counts as neglect
/// never means changing a query.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct GrowthDebtObservation {
    pub kind: GrowthDebtKind,
    pub subject: GrowthDebtSubject,
    /// Hours since the outstanding work was last touched: last interaction in
    /// either direction, last recorded milestone, last contact verification.
    /// The adapter reports the *oldest* evidence it can defend.
    pub idle_hours: u32,
    /// How many tracked items are still outstanding. `1` for a single-subject
    /// kind such as a quiet relationship.
    pub outstanding_items: u32,
    /// How many were tracked in total. Zero means nothing was ever declared,
    /// which is an absence of records rather than evidence of neglect.
    pub tracked_items: u32,
    /// Relationship warmth in `0..=100` where the subject has one.
    pub relationship_score: Option<u8>,
    /// Hours until the subject's own date. Negative once it has passed. `None`
    /// when the kind has no date at all.
    pub hours_until_deadline: Option<i64>,
    /// Hours since this subject last produced an Autopilot decision in this
    /// context. `None` when it never has.
    pub hours_since_last_signal: Option<u32>,
}

/// Tunable thresholds for the `growth_debt` context. Horizons are hours so a
/// single unit covers a 3-day grace period and a 6-month one.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct GrowthDebtPolicy {
    /// A warm relationship is quiet after this long with no interaction.
    pub relationship_quiet_after_hours: u32,
    /// Below this score a quiet contact is not warm enough to be debt.
    pub minimum_relationship_score: u8,
    /// Contact data is stale after this long without re-verification.
    pub contact_data_stale_after_hours: u32,
    /// Declared event surfaces are late once unrequested this long.
    pub event_lever_lead_time_hours: u32,
    /// Release milestones get this much grace before counting as missed.
    pub release_milestone_grace_hours: u32,
    /// Aggregate kinds need at least this share of their tracked items still
    /// outstanding. Protects a nearly finished plan from being called debt.
    pub minimum_outstanding_basis_points: u32,
    /// Hours a subject stays quiet after it produced a decision.
    pub cooldown_hours: u32,
    /// Inside this many hours of its own date, deadline-bound debt is urgent.
    pub deadline_urgency_hours: u32,
}

impl Default for GrowthDebtPolicy {
    fn default() -> Self {
        Self {
            // Two months: long enough that a normal quiet stretch between a
            // tour cycle and the next one is not reported as neglect.
            relationship_quiet_after_hours: 1_440,
            minimum_relationship_score: 60,
            // Six months. Contact rot is real but slow, and re-verification
            // costs a human message.
            contact_data_stale_after_hours: 4_320,
            // Two weeks before the show is the last point at which free
            // distribution still has time to compound.
            event_lever_lead_time_hours: 336,
            release_milestone_grace_hours: 72,
            minimum_outstanding_basis_points: 2_500,
            cooldown_hours: 336,
            deadline_urgency_hours: 168,
        }
    }
}

/// One item of debt worth raising, with the evidence that justifies it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct GrowthDebtItem {
    pub kind: GrowthDebtKind,
    pub confidence: Confidence,
    /// Priority in `0..=100`, combining the kind, how far past its horizon the
    /// work is, how close the subject's own date is, and how much of the work
    /// is still outstanding.
    pub priority: u16,
    /// How far past the horizon the work is, in basis points. `10_000` is
    /// exactly at the horizon; `20_000` is twice as long as allowed. A measured
    /// ratio, never a forecast and never a currency amount.
    pub overdue_basis_points: u32,
    /// Share of tracked items still outstanding, in basis points.
    pub outstanding_basis_points: u32,
    pub outstanding_items: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum GrowthDebtDecision {
    Hold,
    Raise(GrowthDebtItem),
}

/// The horizon a kind is allowed to sit idle for before it is debt.
#[must_use]
pub const fn horizon_hours(kind: GrowthDebtKind, policy: GrowthDebtPolicy) -> u32 {
    match kind {
        GrowthDebtKind::RelationshipQuiet => policy.relationship_quiet_after_hours,
        GrowthDebtKind::EventLeversSkipped => policy.event_lever_lead_time_hours,
        GrowthDebtKind::ReleaseMilestonesMissed => policy.release_milestone_grace_hours,
        GrowthDebtKind::StaleContactData => policy.contact_data_stale_after_hours,
    }
}

fn confidence_from(
    overdue_basis_points: u32,
    outstanding_basis_points: u32,
    tracked_items: u32,
) -> Confidence {
    // A record of absence is a weaker claim than a measured movement, so this
    // starts well below the metric rule's ceiling and earns the rest.
    let overdue_bp = overdue_basis_points.saturating_sub(10_000) / 5;
    let share_bp = outstanding_basis_points / 4;
    // A single tracked item cannot corroborate itself; a wide aggregate can.
    let corroboration = match tracked_items {
        0 | 1 => 0,
        2..=4 => 500,
        _ => 1_000,
    };
    Confidence::saturating_from_basis_points(
        u16::try_from(
            4_000_u32
                .saturating_add(overdue_bp.min(2_500))
                .saturating_add(share_bp.min(2_500))
                .saturating_add(corroboration)
                .min(10_000),
        )
        .unwrap_or(u16::MAX),
    )
}

fn priority_from(
    kind: GrowthDebtKind,
    overdue_basis_points: u32,
    outstanding_basis_points: u32,
    hours_until_deadline: Option<i64>,
    policy: GrowthDebtPolicy,
) -> u16 {
    let magnitude = u16::try_from(overdue_basis_points.saturating_sub(10_000) / 2_000)
        .unwrap_or(u16::MAX)
        .min(8);
    let share = u16::try_from(outstanding_basis_points / 2_000)
        .unwrap_or(u16::MAX)
        .min(5);
    let tier_bonus = kind.value_tier().weight() / 10;
    // Only a date that is still ahead adds urgency. A passed date is handled
    // earlier by dropping the item entirely.
    let urgency = match hours_until_deadline {
        Some(hours) if hours >= 0 && hours <= i64::from(policy.deadline_urgency_hours) => 10,
        _ => 0,
    };
    kind.base_priority()
        .saturating_add(magnitude)
        .saturating_add(share)
        .saturating_add(tier_bonus)
        .saturating_add(urgency)
        .min(100)
}

/// Decides whether one neglected subject is currently worth raising.
///
/// Order matters. The cooldown and the expired deadline are checked before any
/// arithmetic, because both mean the answer cannot be "raise" regardless of how
/// bad the numbers look.
#[must_use]
pub fn evaluate_growth_debt(
    observation: &GrowthDebtObservation,
    policy: GrowthDebtPolicy,
) -> GrowthDebtDecision {
    if observation
        .hours_since_last_signal
        .is_some_and(|hours| hours < policy.cooldown_hours)
    {
        return GrowthDebtDecision::Hold;
    }

    let kind = observation.kind;

    // Work whose date has passed cannot be recovered. Reporting it would be
    // accurate and useless, and it would crowd out debt that can still be paid.
    if kind.is_deadline_bound()
        && observation
            .hours_until_deadline
            .is_none_or(|hours| hours < 0)
    {
        return GrowthDebtDecision::Hold;
    }

    // No denominator, no claim: "nothing was done" is only debt when something
    // was declared in the first place.
    if observation.tracked_items == 0 || observation.outstanding_items == 0 {
        return GrowthDebtDecision::Hold;
    }

    if kind.requires_relationship_score() {
        let Some(score) = observation.relationship_score else {
            return GrowthDebtDecision::Hold;
        };
        if score < policy.minimum_relationship_score {
            return GrowthDebtDecision::Hold;
        }
    }

    let horizon = horizon_hours(kind, policy);
    if horizon == 0 || observation.idle_hours <= horizon {
        return GrowthDebtDecision::Hold;
    }

    let outstanding = observation.outstanding_items.min(observation.tracked_items);
    // Widened deliberately: `outstanding * 10_000` overflows a u32 for any
    // aggregate past ~429k items, and a saturating u32 multiply would then
    // divide a clamped numerator by a real denominator and report a fully
    // neglected subject as ~0% outstanding.
    let outstanding_basis_points = u32::try_from(
        u64::from(outstanding)
            .saturating_mul(10_000)
            .checked_div(u64::from(observation.tracked_items))
            .unwrap_or(0),
    )
    .unwrap_or(u32::MAX)
    .min(10_000);
    if outstanding_basis_points < policy.minimum_outstanding_basis_points {
        return GrowthDebtDecision::Hold;
    }

    let overdue_basis_points = u64::from(observation.idle_hours)
        .saturating_mul(10_000)
        .checked_div(u64::from(horizon))
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(u32::MAX);

    GrowthDebtDecision::Raise(GrowthDebtItem {
        kind,
        confidence: confidence_from(
            overdue_basis_points,
            outstanding_basis_points,
            observation.tracked_items,
        ),
        priority: priority_from(
            kind,
            overdue_basis_points,
            outstanding_basis_points,
            observation.hours_until_deadline,
            policy,
        ),
        overdue_basis_points,
        outstanding_basis_points,
        outstanding_items: outstanding,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn quiet_relationship() -> GrowthDebtObservation {
        GrowthDebtObservation {
            kind: GrowthDebtKind::RelationshipQuiet,
            subject: GrowthDebtSubject::BookingTarget(BookingTargetId::from(Uuid::from_u128(1))),
            idle_hours: 2_000,
            outstanding_items: 1,
            tracked_items: 1,
            relationship_score: Some(80),
            hours_until_deadline: None,
            hours_since_last_signal: None,
        }
    }

    fn skipped_levers() -> GrowthDebtObservation {
        GrowthDebtObservation {
            kind: GrowthDebtKind::EventLeversSkipped,
            subject: GrowthDebtSubject::Event(EventId::from(Uuid::from_u128(2))),
            idle_hours: 500,
            outstanding_items: 6,
            tracked_items: 10,
            relationship_score: None,
            hours_until_deadline: Some(240),
            hours_since_last_signal: None,
        }
    }

    fn raised(decision: GrowthDebtDecision) -> GrowthDebtItem {
        match decision {
            GrowthDebtDecision::Raise(item) => item,
            GrowthDebtDecision::Hold => panic!("expected the rule to raise this observation"),
        }
    }

    #[test]
    fn work_inside_its_horizon_is_not_debt() {
        let observation = GrowthDebtObservation {
            idle_hours: GrowthDebtPolicy::default().relationship_quiet_after_hours,
            ..quiet_relationship()
        };
        assert_eq!(
            evaluate_growth_debt(&observation, GrowthDebtPolicy::default()),
            GrowthDebtDecision::Hold
        );
    }

    #[test]
    fn work_past_its_horizon_is_raised_with_a_measured_overdue_ratio() {
        let item = raised(evaluate_growth_debt(
            &quiet_relationship(),
            GrowthDebtPolicy::default(),
        ));
        assert_eq!(item.kind, GrowthDebtKind::RelationshipQuiet);
        // 2000 idle hours against a 1440-hour horizon.
        assert_eq!(item.overdue_basis_points, 13_888);
        assert_eq!(item.outstanding_basis_points, 10_000);
    }

    #[test]
    fn a_cold_contact_going_quiet_is_not_debt() {
        let observation = GrowthDebtObservation {
            relationship_score: Some(10),
            ..quiet_relationship()
        };
        assert_eq!(
            evaluate_growth_debt(&observation, GrowthDebtPolicy::default()),
            GrowthDebtDecision::Hold
        );
    }

    #[test]
    fn an_unknown_relationship_score_is_thin_evidence_not_debt() {
        let observation = GrowthDebtObservation {
            relationship_score: None,
            ..quiet_relationship()
        };
        assert_eq!(
            evaluate_growth_debt(&observation, GrowthDebtPolicy::default()),
            GrowthDebtDecision::Hold
        );
    }

    #[test]
    fn debt_whose_deadline_has_passed_is_dropped() {
        let observation = GrowthDebtObservation {
            hours_until_deadline: Some(-1),
            ..skipped_levers()
        };
        assert_eq!(
            evaluate_growth_debt(&observation, GrowthDebtPolicy::default()),
            GrowthDebtDecision::Hold
        );
    }

    #[test]
    fn deadline_bound_debt_without_a_date_is_dropped() {
        let observation = GrowthDebtObservation {
            hours_until_deadline: None,
            ..skipped_levers()
        };
        assert_eq!(
            evaluate_growth_debt(&observation, GrowthDebtPolicy::default()),
            GrowthDebtDecision::Hold
        );
    }

    #[test]
    fn nothing_tracked_is_never_reported_as_everything_neglected() {
        let observation = GrowthDebtObservation {
            outstanding_items: 0,
            tracked_items: 0,
            ..skipped_levers()
        };
        assert_eq!(
            evaluate_growth_debt(&observation, GrowthDebtPolicy::default()),
            GrowthDebtDecision::Hold
        );
    }

    #[test]
    fn a_nearly_finished_plan_is_not_debt() {
        let observation = GrowthDebtObservation {
            outstanding_items: 1,
            tracked_items: 10,
            ..skipped_levers()
        };
        assert_eq!(
            evaluate_growth_debt(&observation, GrowthDebtPolicy::default()),
            GrowthDebtDecision::Hold
        );
    }

    #[test]
    fn a_subject_inside_its_cooldown_stays_quiet() {
        let policy = GrowthDebtPolicy::default();
        let observation = GrowthDebtObservation {
            hours_since_last_signal: Some(policy.cooldown_hours - 1),
            ..quiet_relationship()
        };
        assert_eq!(
            evaluate_growth_debt(&observation, policy),
            GrowthDebtDecision::Hold
        );

        let observation = GrowthDebtObservation {
            hours_since_last_signal: Some(policy.cooldown_hours),
            ..quiet_relationship()
        };
        assert!(matches!(
            evaluate_growth_debt(&observation, policy),
            GrowthDebtDecision::Raise(_)
        ));
    }

    #[test]
    fn downstream_debt_outranks_hygiene_debt_at_the_same_overdue_ratio() {
        let policy = GrowthDebtPolicy::default();
        let levers = raised(evaluate_growth_debt(
            &GrowthDebtObservation {
                // Exactly twice its horizon, and far enough out that the
                // deadline bonus does not decide the comparison.
                idle_hours: policy.event_lever_lead_time_hours * 2,
                hours_until_deadline: Some(1_000),
                ..skipped_levers()
            },
            policy,
        ));
        let hygiene = raised(evaluate_growth_debt(
            &GrowthDebtObservation {
                kind: GrowthDebtKind::StaleContactData,
                idle_hours: policy.contact_data_stale_after_hours * 2,
                relationship_score: None,
                hours_until_deadline: None,
                ..quiet_relationship()
            },
            policy,
        ));
        assert_eq!(levers.overdue_basis_points, hygiene.overdue_basis_points);
        assert!(levers.priority > hygiene.priority);
    }

    #[test]
    fn an_imminent_deadline_raises_priority() {
        let policy = GrowthDebtPolicy::default();
        let far = raised(evaluate_growth_debt(
            &GrowthDebtObservation {
                hours_until_deadline: Some(i64::from(policy.deadline_urgency_hours) + 1),
                ..skipped_levers()
            },
            policy,
        ));
        let near = raised(evaluate_growth_debt(
            &GrowthDebtObservation {
                hours_until_deadline: Some(i64::from(policy.deadline_urgency_hours)),
                ..skipped_levers()
            },
            policy,
        ));
        assert!(near.priority > far.priority);
    }

    #[test]
    fn confidence_grows_with_overdue_share_and_corroboration() {
        let policy = GrowthDebtPolicy::default();
        let single = raised(evaluate_growth_debt(&quiet_relationship(), policy));
        let wide = raised(evaluate_growth_debt(
            &GrowthDebtObservation {
                idle_hours: policy.event_lever_lead_time_hours * 3,
                outstanding_items: 10,
                tracked_items: 10,
                ..skipped_levers()
            },
            policy,
        ));
        assert!(wide.confidence > single.confidence);
        assert!(single.confidence < Confidence::MAX);
    }

    #[test]
    fn priority_and_confidence_stay_inside_their_ranges() {
        let policy = GrowthDebtPolicy::default();
        let item = raised(evaluate_growth_debt(
            &GrowthDebtObservation {
                idle_hours: u32::MAX,
                outstanding_items: u32::MAX,
                tracked_items: u32::MAX,
                hours_until_deadline: Some(1),
                ..skipped_levers()
            },
            policy,
        ));
        assert!(item.priority <= 100);
        assert!(item.confidence <= Confidence::MAX);
        assert_eq!(item.outstanding_basis_points, 10_000);
    }

    #[test]
    fn every_kind_round_trips_through_its_wire_name() {
        for kind in [
            GrowthDebtKind::RelationshipQuiet,
            GrowthDebtKind::EventLeversSkipped,
            GrowthDebtKind::ReleaseMilestonesMissed,
            GrowthDebtKind::StaleContactData,
        ] {
            assert_eq!(GrowthDebtKind::parse(kind.as_str()), Some(kind));
            assert!(!kind.reason().is_empty());
            assert!(!kind.recommended_action().is_empty());
            assert!(!kind.template_key().is_empty());
        }
        assert_eq!(GrowthDebtKind::parse("unknown"), None);
    }

    #[test]
    fn a_zero_horizon_never_reports_every_subject_as_debt() {
        let policy = GrowthDebtPolicy {
            relationship_quiet_after_hours: 0,
            ..GrowthDebtPolicy::default()
        };
        assert_eq!(
            evaluate_growth_debt(&quiet_relationship(), policy),
            GrowthDebtDecision::Hold
        );
    }
}
