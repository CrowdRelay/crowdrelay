//! What the band is trying to achieve, so the agent has somewhere to be.
//!
//! Every context before this reacts: a series stalled, a step is due, a
//! pipeline is empty. None of them can say whether the work adds up to
//! anything, because nothing declares what "anything" would be.
//!
//! An objective is an operator's target on a measured series — a value, a
//! deadline and a scope. Three rules keep it from becoming self-congratulation.
//!
//! 1. **An objective is never evidence of progress.** Its state comes from the
//!    series and from nothing else. Actions taken toward it, however many, move
//!    the number zero.
//! 2. **A missed objective is reported as missed.** It does not quietly expire,
//!    roll forward or become "in progress" again. The deadline passing with the
//!    target unmet is a fact, and it is the fact most worth surfacing.
//! 3. **A projection refuses more readily than it guesses.** With no baseline,
//!    no observation, or too little elapsed time to imply a pace, the objective
//!    is [`ObjectiveState::Unmeasurable`] with the reason — not "on track".

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{CityId, EventId, ReleasePlanId, growth_metrics::MetricDirection};

/// What an objective is scoped to.
///
/// A workspace target and a city target are different promises, and merging
/// them would let a national number hide a city where nothing is happening.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum ObjectiveScope {
    Workspace,
    City(CityId),
    Event(EventId),
    ReleasePlan(ReleasePlanId),
}

impl ObjectiveScope {
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::City(_) => "city",
            Self::Event(_) => "event",
            Self::ReleasePlan(_) => "release_plan",
        }
    }

    #[must_use]
    pub fn subject_id(self) -> Option<uuid::Uuid> {
        match self {
            Self::Workspace => None,
            Self::City(id) => Some(id.into_uuid()),
            Self::Event(id) => Some(id.into_uuid()),
            Self::ReleasePlan(id) => Some(id.into_uuid()),
        }
    }
}

/// An operator's declared target.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GrowthObjective {
    pub platform: String,
    pub metric_key: String,
    pub scope: ObjectiveScope,
    pub direction: MetricDirection,
    /// Where the series stood when the objective was declared. Frozen, because
    /// progress measured from a baseline that moves is not progress.
    pub baseline_value: i64,
    pub target_value: i64,
    pub declared_at: OffsetDateTime,
    pub deadline: OffsetDateTime,
}

/// Why an objective cannot be judged.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectiveGap {
    /// The series has no observation, so there is nothing to compare.
    NoObservation,
    /// Too little time has elapsed to imply a pace. Projecting from a few hours
    /// produces a number with a decimal point and no meaning.
    TooEarlyToProject,
    /// The target equals the baseline, so there is no distance to cover and no
    /// progress to express.
    NoDistanceToCover,
}

impl ObjectiveGap {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoObservation => "no_observation",
            Self::TooEarlyToProject => "too_early_to_project",
            Self::NoDistanceToCover => "no_distance_to_cover",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "no_observation" => Some(Self::NoObservation),
            "too_early_to_project" => Some(Self::TooEarlyToProject),
            "no_distance_to_cover" => Some(Self::NoDistanceToCover),
            _ => None,
        }
    }
}

/// Where an objective stands, measured only from its series.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ObjectiveState {
    /// The target has been reached. Terminal: a series that later falls back
    /// does not un-meet an objective that was met.
    Met {
        progress_basis_points: u32,
    },
    /// At the pace since it was declared, the target arrives before the
    /// deadline.
    OnTrack {
        progress_basis_points: u32,
        projected_value: i64,
    },
    /// At that pace it does not.
    Behind {
        progress_basis_points: u32,
        projected_value: i64,
        shortfall: i64,
    },
    /// The deadline passed with the target unmet. Terminal, and stated.
    Missed {
        progress_basis_points: u32,
        final_value: i64,
        shortfall: i64,
    },
    Unmeasurable {
        reason: ObjectiveGap,
    },
}

impl ObjectiveState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Met { .. } => "met",
            Self::OnTrack { .. } => "on_track",
            Self::Behind { .. } => "behind",
            Self::Missed { .. } => "missed",
            Self::Unmeasurable { .. } => "unmeasurable",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<&'static str> {
        ["met", "on_track", "behind", "missed", "unmeasurable"]
            .into_iter()
            .find(|known| *known == value)
    }

    /// True while the objective is still something the agent should be working
    /// toward. A met or missed objective is history, and history must not keep
    /// promoting work up the queue.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::OnTrack { .. } | Self::Behind { .. })
    }

    /// True when an operator should be told without being asked.
    #[must_use]
    pub const fn warrants_attention(self) -> bool {
        matches!(self, Self::Behind { .. } | Self::Missed { .. })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ObjectivePolicy {
    /// Elapsed time below which no pace is inferred.
    pub minimum_elapsed_hours: u32,
}

impl Default for ObjectivePolicy {
    fn default() -> Self {
        Self {
            // Three days. A weekly series cannot imply a pace from less, and a
            // projection from one observation is arithmetic rather than
            // evidence.
            minimum_elapsed_hours: 72,
        }
    }
}

/// Where an objective stands, given the latest observation of its series.
///
/// `observed` is the series value and the moment it describes. `None` means the
/// series has nothing to say, which is reported rather than treated as no
/// movement.
#[must_use]
pub fn assess_objective(
    objective: &GrowthObjective,
    observed: Option<(i64, OffsetDateTime)>,
    policy: ObjectivePolicy,
    now: OffsetDateTime,
) -> ObjectiveState {
    let Some((observed_value, observed_at)) = observed else {
        return ObjectiveState::Unmeasurable {
            reason: ObjectiveGap::NoObservation,
        };
    };
    // Oriented so that "further" always means "closer to the target", whether
    // the target is above the baseline or below it.
    let distance = objective.direction.orient(
        objective
            .target_value
            .saturating_sub(objective.baseline_value),
    );
    if distance == 0 {
        return ObjectiveState::Unmeasurable {
            reason: ObjectiveGap::NoDistanceToCover,
        };
    }
    let travelled = objective
        .direction
        .orient(observed_value.saturating_sub(objective.baseline_value));
    let progress_basis_points = progress(travelled, distance);

    if travelled >= distance {
        // Reached. Terminal on purpose: a series that later falls back does not
        // un-meet an objective that was met, and rewriting history would make
        // every past report unreliable.
        return ObjectiveState::Met {
            progress_basis_points,
        };
    }
    let shortfall = objective
        .direction
        .orient(objective.target_value.saturating_sub(observed_value))
        .max(0);
    if now >= objective.deadline {
        return ObjectiveState::Missed {
            progress_basis_points,
            final_value: observed_value,
            shortfall,
        };
    }

    let elapsed = observed_at - objective.declared_at;
    if elapsed < Duration::hours(i64::from(policy.minimum_elapsed_hours)) {
        return ObjectiveState::Unmeasurable {
            reason: ObjectiveGap::TooEarlyToProject,
        };
    }
    let Some(projected) = project(objective, travelled, elapsed) else {
        return ObjectiveState::Unmeasurable {
            reason: ObjectiveGap::TooEarlyToProject,
        };
    };
    if objective.direction.orient(projected) >= objective.direction.orient(objective.target_value) {
        ObjectiveState::OnTrack {
            progress_basis_points,
            projected_value: projected,
        }
    } else {
        ObjectiveState::Behind {
            progress_basis_points,
            projected_value: projected,
            shortfall,
        }
    }
}

/// The value the current pace reaches by the deadline.
fn project(objective: &GrowthObjective, travelled: i64, elapsed: Duration) -> Option<i64> {
    let elapsed_seconds = elapsed.whole_seconds();
    let whole_seconds = (objective.deadline - objective.declared_at).whole_seconds();
    if elapsed_seconds <= 0 || whole_seconds <= 0 {
        return None;
    }
    let projected_travel =
        i128::from(travelled) * i128::from(whole_seconds) / i128::from(elapsed_seconds);
    let signed = match objective.direction {
        MetricDirection::HigherIsBetter => projected_travel,
        MetricDirection::LowerIsBetter => -projected_travel,
    };
    i64::try_from(i128::from(objective.baseline_value) + signed).ok()
}

/// How far along, capped at complete. Never negative: an objective that has
/// gone backwards is at zero progress, and a negative percentage is a number
/// nobody asked for.
fn progress(travelled: i64, distance: i64) -> u32 {
    if distance <= 0 || travelled <= 0 {
        return 0;
    }
    let raw = i128::from(travelled) * 10_000 / i128::from(distance);
    u32::try_from(raw.min(10_000)).unwrap_or(10_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moment(days: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000 + days)
    }

    fn objective(direction: MetricDirection, baseline: i64, target: i64) -> GrowthObjective {
        GrowthObjective {
            platform: "bandsintown".to_owned(),
            metric_key: "trackers".to_owned(),
            scope: ObjectiveScope::Workspace,
            direction,
            baseline_value: baseline,
            target_value: target,
            declared_at: moment(0),
            deadline: moment(100),
        }
    }

    fn policy() -> ObjectivePolicy {
        ObjectivePolicy::default()
    }

    #[test]
    fn a_pace_that_arrives_before_the_deadline_is_on_track() {
        let state = assess_objective(
            &objective(MetricDirection::HigherIsBetter, 100, 200),
            Some((150, moment(50))),
            policy(),
            moment(50),
        );
        assert!(matches!(state, ObjectiveState::OnTrack { .. }));
        assert!(state.is_active());
    }

    #[test]
    fn a_pace_that_does_not_is_behind_and_says_by_how_much() {
        let state = assess_objective(
            &objective(MetricDirection::HigherIsBetter, 100, 200),
            Some((110, moment(50))),
            policy(),
            moment(50),
        );
        let ObjectiveState::Behind {
            shortfall,
            projected_value,
            ..
        } = state
        else {
            panic!("ten of a hundred in half the time does not arrive");
        };
        assert_eq!(shortfall, 90);
        assert_eq!(projected_value, 120);
        assert!(state.warrants_attention());
    }

    #[test]
    fn a_deadline_that_passed_unmet_is_missed_and_stays_missed() {
        let state = assess_objective(
            &objective(MetricDirection::HigherIsBetter, 100, 200),
            Some((160, moment(120))),
            policy(),
            moment(120),
        );
        let ObjectiveState::Missed {
            final_value,
            shortfall,
            progress_basis_points,
        } = state
        else {
            panic!("the deadline passed with the target unmet");
        };
        assert_eq!(final_value, 160);
        assert_eq!(shortfall, 40);
        assert_eq!(progress_basis_points, 6_000);
        assert!(!state.is_active(), "history must not keep promoting work");
        assert!(state.warrants_attention());
    }

    #[test]
    fn a_met_objective_is_not_unmet_by_a_later_fall() {
        // Reached at day 50, series falls back by day 60. The objective was met.
        let met = assess_objective(
            &objective(MetricDirection::HigherIsBetter, 100, 200),
            Some((210, moment(50))),
            policy(),
            moment(50),
        );
        assert!(matches!(met, ObjectiveState::Met { .. }));
        assert!(!met.is_active());
        assert!(!met.warrants_attention());
    }

    #[test]
    fn a_series_with_nothing_to_say_is_unmeasurable_and_not_on_track() {
        assert_eq!(
            assess_objective(
                &objective(MetricDirection::HigherIsBetter, 100, 200),
                None,
                policy(),
                moment(50),
            ),
            ObjectiveState::Unmeasurable {
                reason: ObjectiveGap::NoObservation
            }
        );
    }

    #[test]
    fn a_projection_from_a_few_hours_is_refused() {
        assert_eq!(
            assess_objective(
                &objective(MetricDirection::HigherIsBetter, 100, 200),
                Some((101, moment(0) + Duration::hours(6))),
                policy(),
                moment(1),
            )
            .as_str(),
            "unmeasurable"
        );
    }

    #[test]
    fn a_target_equal_to_the_baseline_has_no_progress_to_express() {
        assert_eq!(
            assess_objective(
                &objective(MetricDirection::HigherIsBetter, 100, 100),
                Some((100, moment(50))),
                policy(),
                moment(50),
            ),
            ObjectiveState::Unmeasurable {
                reason: ObjectiveGap::NoDistanceToCover
            }
        );
    }

    #[test]
    fn a_target_where_falling_is_good_is_judged_on_its_own_terms() {
        // Unsubscribes: 500 down to 200 by the deadline.
        let falling = objective(MetricDirection::LowerIsBetter, 500, 200);
        let state = assess_objective(&falling, Some((350, moment(50))), policy(), moment(50));
        assert!(
            matches!(state, ObjectiveState::OnTrack { .. }),
            "halfway down in half the time arrives, got {state:?}"
        );
        let met = assess_objective(&falling, Some((150, moment(50))), policy(), moment(50));
        assert!(matches!(met, ObjectiveState::Met { .. }));
    }

    #[test]
    fn going_backwards_is_zero_progress_and_never_a_negative_percentage() {
        let state = assess_objective(
            &objective(MetricDirection::HigherIsBetter, 100, 200),
            Some((60, moment(50))),
            policy(),
            moment(50),
        );
        let ObjectiveState::Behind {
            progress_basis_points,
            ..
        } = state
        else {
            panic!("a series that fell is behind");
        };
        assert_eq!(progress_basis_points, 0);
    }

    #[test]
    fn every_gap_and_state_names_itself() {
        for gap in [
            ObjectiveGap::NoObservation,
            ObjectiveGap::TooEarlyToProject,
            ObjectiveGap::NoDistanceToCover,
        ] {
            assert_eq!(ObjectiveGap::parse(gap.as_str()), Some(gap));
        }
        for state in ["met", "on_track", "behind", "missed", "unmeasurable"] {
            assert_eq!(ObjectiveState::parse(state), Some(state));
        }
        assert_eq!(ObjectiveScope::Workspace.subject_id(), None);
        assert_eq!(ObjectiveScope::Workspace.kind(), "workspace");
    }
}
