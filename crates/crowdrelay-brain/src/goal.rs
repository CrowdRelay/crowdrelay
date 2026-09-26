//! Goal-directed control — what an operator's objective is allowed to change.
//!
//! An operator declares "100 durable fans by the 27th" as a
//! [`GrowthObjective`](crowdrelay_domain::objectives::GrowthObjective), and
//! `assess_objective` judges it from its own series. Until this module, that
//! judgement reached the API and the chief briefing and no decision: the brain
//! worked toward a derived monthly bucket and an objective turning `Behind`
//! changed nothing it did.
//!
//! The contract is `docs/GOAL_DIRECTED_CONTROL.md`, and its limits are the
//! load-bearing half:
//!
//! - **Urgency is not value.** Nothing here reaches `DecisionValue::total()`.
//!   A deadline does not make a bad action better; a candidate is worth what it
//!   is expected to produce.
//! - **A constraint and a posture, nothing else.** A `Behind` objective may
//!   raise `PortfolioConfig::max_dispatches` up to an operator-configured
//!   ceiling, and may withhold the metacognition exploration boost so the
//!   brain exploits rather than explores. Every candidate still has to clear
//!   `min_marginal_value` alone.
//! - **No second definition of progress.** The state is `assess_objective`'s,
//!   carried here unchanged. This module derives a pace from it and does not
//!   re-judge it.
//! - **No safety override.** Authority, approvals, class ceilings, envelope
//!   budgets and circuit breakers sit downstream of the portfolio and are not
//!   read or written here.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crowdrelay_domain::growth_metrics::MetricDirection;
use crowdrelay_domain::objectives::ObjectiveState;

/// The operator's objective the brain is working toward this cycle, as
/// `assess_objective` judged it.
///
/// Workspace-scoped only. A city or release-plan objective is a promise about
/// part of the audience, and the portfolio selects for the whole workspace — a
/// city being behind is not a reason to send more everywhere.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveObjective {
    pub objective_id: uuid::Uuid,
    pub platform: String,
    pub metric_key: String,
    pub direction: MetricDirection,
    /// Frozen at declaration.
    pub baseline_value: i64,
    pub target_value: i64,
    /// The latest value of the series. `None` is reported, not zero.
    pub observed_value: Option<i64>,
    #[serde(with = "time::serde::rfc3339")]
    pub declared_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub deadline: OffsetDateTime,
    /// `assess_objective`'s verdict — the only definition of progress.
    pub state: ObjectiveState,
}

impl ActiveObjective {
    /// True when the objective's own assessment says the target will not
    /// arrive in time at the observed pace — the brain's cue to exploit.
    #[must_use]
    pub const fn is_behind(&self) -> bool {
        matches!(self.state, ObjectiveState::Behind { .. })
    }

    /// The one objective the brain optimises for: the live one whose deadline
    /// comes first. Met, missed and unmeasurable objectives are history or
    /// unknown, and neither may steer a decision. Ties break on id so the
    /// choice does not depend on load order.
    #[must_use]
    pub fn choose(candidates: impl IntoIterator<Item = Self>) -> Option<Self> {
        candidates
            .into_iter()
            .filter(|objective| objective.state.is_active())
            .min_by(|a, b| {
                a.deadline
                    .cmp(&b.deadline)
                    .then_with(|| a.objective_id.cmp(&b.objective_id))
            })
    }
}

/// How the brain stands against the objective.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalPosture {
    /// At the observed pace the target arrives in time. Nothing changes: an
    /// objective that is being met is not a reason to act differently.
    OnTrack,
    /// It does not. The pace may raise the dispatch ceiling and the brain
    /// exploits rather than explores.
    Behind,
}

/// The pace an objective implies, in the objective's own series units per day.
///
/// Arithmetic on `assess_objective`'s output and the deadline, not a model.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GoalPace {
    pub objective_id: uuid::Uuid,
    pub posture: GoalPosture,
    /// Distance still to cover, oriented so positive is toward the target.
    pub remaining: i64,
    /// Days until the deadline. Strictly positive: a passed deadline is
    /// `Missed`, which is not active and never produces a pace.
    pub days_remaining: f64,
    /// `remaining / days_remaining`.
    pub required_per_day: f64,
    /// Distance travelled since declaration over days elapsed. `None` before
    /// any time has elapsed.
    pub observed_per_day: Option<f64>,
}

impl GoalPace {
    /// The pace an active objective implies, or `None` when it is not active
    /// or has no observation — the brain does not guess a pace for a series it
    /// cannot see.
    #[must_use]
    pub fn from_objective(objective: &ActiveObjective, now: OffsetDateTime) -> Option<Self> {
        let posture = match objective.state {
            ObjectiveState::OnTrack { .. } => GoalPosture::OnTrack,
            ObjectiveState::Behind { .. } => GoalPosture::Behind,
            _ => return None,
        };
        let observed = objective.observed_value?;
        let seconds_left = (objective.deadline - now).whole_seconds();
        if seconds_left <= 0 {
            return None;
        }
        let days_remaining = seconds_left as f64 / 86_400.0;
        let remaining = match objective.state {
            // The assessment's own shortfall when it states one, so there is
            // exactly one answer to "how far is left".
            ObjectiveState::Behind { shortfall, .. } => shortfall,
            _ => objective
                .direction
                .orient(objective.target_value.saturating_sub(observed))
                .max(0),
        };
        let travelled = objective
            .direction
            .orient(observed.saturating_sub(objective.baseline_value));
        let seconds_elapsed = (now - objective.declared_at).whole_seconds();
        let observed_per_day =
            (seconds_elapsed > 0).then(|| travelled as f64 / (seconds_elapsed as f64 / 86_400.0));
        Some(Self {
            objective_id: objective.objective_id,
            posture,
            remaining,
            days_remaining,
            required_per_day: remaining as f64 / days_remaining,
            observed_per_day,
        })
    }

    /// How many times faster than the observed pace the objective needs the
    /// brain to go. `None` when there is no positive observed pace to divide
    /// by — no movement, or movement the wrong way — which reads as "as fast
    /// as the ceiling allows" rather than a number with no denominator.
    #[must_use]
    pub fn pace_ratio(&self) -> Option<f64> {
        self.observed_per_day
            .filter(|observed| *observed > 0.0)
            .map(|observed| self.required_per_day / observed)
    }

    /// True when the brain should exploit rather than explore. Behind and short
    /// on time is a reason to spend on what is known to work; it changes which
    /// candidates are generated, never what any of them is worth.
    #[must_use]
    pub const fn suppresses_exploration(&self) -> bool {
        matches!(self.posture, GoalPosture::Behind)
    }

    /// The dispatch ceiling this pace allows.
    ///
    /// `base` is the cycle's already-sized budget and `ceiling` the most the
    /// operator allows under a deadline, sized the same way. On track, the
    /// base stands. Behind, the base is scaled by the pace ratio — the gap
    /// between the pace needed and the pace observed — and clamped to the
    /// ceiling. The result is never below `base`: a goal may widen the budget,
    /// never shrink it, because shrinking is metacognition's and execution
    /// health's call and a deadline is not evidence about either.
    #[must_use]
    pub fn max_dispatches(&self, base: u32, ceiling: u32) -> u32 {
        if self.posture != GoalPosture::Behind {
            return base;
        }
        let ceiling = ceiling.max(base);
        match self.pace_ratio() {
            Some(ratio) if ratio.is_finite() => {
                let raised = (f64::from(base) * ratio).ceil();
                if raised >= f64::from(ceiling) {
                    ceiling
                } else {
                    (raised as u32).clamp(base, ceiling)
                }
            }
            _ => ceiling,
        }
    }
}

/// What the goal did to one cycle's portfolio, recorded on every decision the
/// cycle writes so "the brain sent more because it was behind" is a fact on
/// the decision rather than something re-derived later against a series that
/// has since moved.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GoalConstraint {
    pub objective_id: uuid::Uuid,
    pub platform: String,
    pub metric_key: String,
    pub state: ObjectiveState,
    pub pace: GoalPace,
    /// `max_dispatches` before the goal was applied.
    pub base_max_dispatches: u32,
    /// `max_dispatches` the optimizer ran with.
    pub applied_max_dispatches: u32,
    /// Whether the metacognition exploration boost was withheld.
    pub exploration_suppressed: bool,
}

impl GoalConstraint {
    /// Applies the objective's pace to the cycle's sized dispatch budget.
    #[must_use]
    pub fn apply(
        objective: &ActiveObjective,
        now: OffsetDateTime,
        base_max_dispatches: u32,
        goal_ceiling: u32,
    ) -> Option<Self> {
        let pace = GoalPace::from_objective(objective, now)?;
        Some(Self {
            objective_id: objective.objective_id,
            platform: objective.platform.clone(),
            metric_key: objective.metric_key.clone(),
            state: objective.state,
            pace,
            base_max_dispatches,
            applied_max_dispatches: pace.max_dispatches(base_max_dispatches, goal_ceiling),
            exploration_suppressed: pace.suppresses_exploration(),
        })
    }

    /// One line for the cycle report: what the goal needed and what it changed.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "goal {} {}:{} {}: needs {:.2}/day over {:.1} days, observed {}; \
             max_dispatches {} -> {}, exploration boost {}",
            self.objective_id,
            self.platform,
            self.metric_key,
            self.state.as_str(),
            self.pace.required_per_day,
            self.pace.days_remaining,
            self.pace
                .observed_per_day
                .map_or_else(|| "none".to_owned(), |pace| format!("{pace:.2}/day")),
            self.base_max_dispatches,
            self.applied_max_dispatches,
            if self.exploration_suppressed {
                "withheld"
            } else {
                "kept"
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    fn moment(days: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000 + days)
    }

    fn objective(state: ObjectiveState, observed: Option<i64>) -> ActiveObjective {
        ActiveObjective {
            objective_id: uuid::Uuid::from_u128(1),
            platform: "signal".to_owned(),
            metric_key: "activated_fans_30d".to_owned(),
            direction: MetricDirection::HigherIsBetter,
            baseline_value: 0,
            target_value: 100,
            observed_value: observed,
            declared_at: moment(0),
            deadline: moment(21),
            state,
        }
    }

    fn behind(shortfall: i64) -> ObjectiveState {
        ObjectiveState::Behind {
            progress_basis_points: 0,
            projected_value: 0,
            shortfall,
        }
    }

    fn on_track() -> ObjectiveState {
        ObjectiveState::OnTrack {
            progress_basis_points: 0,
            projected_value: 100,
        }
    }

    #[test]
    fn a_hundred_fans_in_21_days_behind_at_day_7_needs_the_pace_it_says() {
        // 10 of 100 after 7 days: 1.43/day observed, 90 over 14 days needed.
        let pace = GoalPace::from_objective(&objective(behind(90), Some(10)), moment(7))
            .expect("an active objective with an observation has a pace");
        assert_eq!(pace.posture, GoalPosture::Behind);
        assert_eq!(pace.remaining, 90);
        assert!((pace.days_remaining - 14.0).abs() < 1e-9);
        assert!((pace.required_per_day - 90.0 / 14.0).abs() < 1e-9);
        let ratio = pace.pace_ratio().expect("positive observed pace");
        assert!((ratio - (90.0 / 14.0) / (10.0 / 7.0)).abs() < 1e-9);
        assert!(pace.suppresses_exploration());
    }

    #[test]
    fn behind_raises_the_budget_by_the_pace_gap_and_never_past_the_ceiling() {
        let pace =
            GoalPace::from_objective(&objective(behind(90), Some(10)), moment(7)).expect("pace");
        // ratio 4.5 × 5 = 22.5 → ceiling 10.
        assert_eq!(pace.max_dispatches(5, 10), 10);
        // A smaller gap raises less: 30 in 10 days is 3/day, 70 in 11 days
        // needs 6.36/day, so ratio 2.12 × 5 → 11 under a ceiling of 20.
        let mild =
            GoalPace::from_objective(&objective(behind(70), Some(30)), moment(10)).expect("pace");
        assert_eq!(mild.max_dispatches(5, 20), 11);
    }

    #[test]
    fn on_track_changes_nothing() {
        let pace =
            GoalPace::from_objective(&objective(on_track(), Some(60)), moment(7)).expect("pace");
        assert_eq!(pace.posture, GoalPosture::OnTrack);
        assert_eq!(pace.max_dispatches(5, 10), 5);
        assert!(!pace.suppresses_exploration());
    }

    #[test]
    fn a_goal_never_shrinks_the_sized_budget() {
        // Metacognition sized the base to 2; a ceiling configured below it is
        // not a reason to go lower.
        let pace =
            GoalPace::from_objective(&objective(behind(90), Some(10)), moment(7)).expect("pace");
        assert_eq!(pace.max_dispatches(8, 3), 8);
    }

    #[test]
    fn no_movement_behind_goes_to_the_ceiling_rather_than_dividing_by_zero() {
        let pace =
            GoalPace::from_objective(&objective(behind(100), Some(0)), moment(7)).expect("pace");
        assert_eq!(pace.pace_ratio(), None);
        assert_eq!(pace.max_dispatches(5, 10), 10);
    }

    #[test]
    fn history_and_unknowns_produce_no_pace() {
        for state in [
            ObjectiveState::Met {
                progress_basis_points: 10_000,
            },
            ObjectiveState::Missed {
                progress_basis_points: 0,
                final_value: 0,
                shortfall: 100,
            },
            ObjectiveState::Unmeasurable {
                reason: crowdrelay_domain::objectives::ObjectiveGap::TooEarlyToProject,
            },
        ] {
            assert!(GoalPace::from_objective(&objective(state, Some(10)), moment(7)).is_none());
        }
        // Active but unobserved: no guess.
        assert!(GoalPace::from_objective(&objective(behind(90), None), moment(7)).is_none());
        // Active but at or past the deadline: no pace.
        assert!(GoalPace::from_objective(&objective(behind(90), Some(10)), moment(21)).is_none());
    }

    #[test]
    fn the_brain_works_toward_the_nearest_live_deadline() {
        let mut near = objective(behind(90), Some(10));
        near.objective_id = uuid::Uuid::from_u128(2);
        near.deadline = moment(14);
        let mut met = objective(
            ObjectiveState::Met {
                progress_basis_points: 10_000,
            },
            Some(100),
        );
        met.deadline = moment(3);
        let far = objective(on_track(), Some(10));
        let chosen = ActiveObjective::choose([far, met, near.clone()]).expect("one is live");
        assert_eq!(chosen.objective_id, near.objective_id);
    }

    #[test]
    fn the_constraint_records_what_it_changed() {
        let constraint = GoalConstraint::apply(&objective(behind(90), Some(10)), moment(7), 5, 10)
            .expect("behind with an observation");
        assert_eq!(constraint.base_max_dispatches, 5);
        assert_eq!(constraint.applied_max_dispatches, 10);
        assert!(constraint.exploration_suppressed);
        let json = serde_json::to_value(&constraint).expect("serialises");
        assert_eq!(json["state"]["state"], "behind");
        assert_eq!(json["pace"]["posture"], "behind");
        assert!(
            constraint
                .summary()
                .ends_with("max_dispatches 5 -> 10, exploration boost withheld"),
            "{}",
            constraint.summary()
        );
    }
}
