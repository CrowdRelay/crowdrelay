//! Treatment assignment for evidence recording.
//!
//! The brain records whether each dispatch was treatment (dispatched) or
//! control (withheld) as part of the evidence row. This enables off-policy
//! evaluation of dispatch decisions.
//!
//! # Experiment Assignments
//!
//! First-class experiment assignments replace the old workspace-wide holdout.
//! The experimental unit is explicitly defined — it can be an audience, a
//! target community, a campaign, a cohort, a city, or the workspace. The
//! unit must be isolatable: if other treatment actions continue on the same
//! unit, the control is contaminated and the evidence quality is downgraded.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::causal_model::{DispatchContext, DispatchPrediction};

/// The treatment assignment for a dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreatmentAssignment {
    /// The dispatch was performed (treatment group).
    Treatment,
    /// The dispatch was withheld (control group).
    Control,
}

impl TreatmentAssignment {
    /// Returns 1.0 for treatment, 0.0 for control — used in IPW calculations.
    #[must_use]
    pub const fn indicator(self) -> f64 {
        match self {
            Self::Treatment => 1.0,
            Self::Control => 0.0,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Treatment => "treatment",
            Self::Control => "control",
        }
    }
}

/// The kind of experimental unit being randomized. The unit must be
/// isolatable — if other treatment actions continue on the same unit,
/// the control is contaminated and evidence quality is downgraded.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentUnitKind {
    /// A specific audience segment (e.g. "metal-fans-berlin").
    Audience,
    /// A target community (e.g. a specific subreddit).
    TargetCommunity,
    /// A campaign or coordinated action batch.
    Campaign,
    /// A cohort of fans (e.g. "new-fans-2026-08").
    Cohort,
    /// A geographic city.
    City,
    /// The entire workspace — the coarsest unit, most prone to contamination.
    Workspace,
}

impl ExperimentUnitKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Audience => "audience",
            Self::TargetCommunity => "target_community",
            Self::Campaign => "campaign",
            Self::Cohort => "cohort",
            Self::City => "city",
            Self::Workspace => "workspace",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "audience" => Some(Self::Audience),
            "target_community" => Some(Self::TargetCommunity),
            "campaign" => Some(Self::Campaign),
            "cohort" => Some(Self::Cohort),
            "city" => Some(Self::City),
            "workspace" => Some(Self::Workspace),
            _ => None,
        }
    }
}

/// The kind of experiment, derived from interference controllability.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentKind {
    /// True randomized holdout: the unit can remain untreated and
    /// contamination is measurable. Full causal strength.
    RandomizedHoldout,
    /// Matched quasi-experiment (DiD): the unit cannot be fully isolated,
    /// so we use difference-in-differences instead of pretending it's clean.
    MatchedQuasiExperiment,
}

impl ExperimentKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RandomizedHoldout => "randomized_holdout",
            Self::MatchedQuasiExperiment => "matched_quasi_experiment",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "randomized_holdout" => Some(Self::RandomizedHoldout),
            "matched_quasi_experiment" => Some(Self::MatchedQuasiExperiment),
            _ => None,
        }
    }
}

/// A first-class experiment assignment — the unit being randomized, the
/// arm, and the metadata needed for causal inference.
///
/// This replaces the old workspace-wide holdout. The experimental unit is
/// explicitly defined and must be isolatable. When contamination is high
/// (other treatment actions continue on the same unit), the experiment
/// kind is downgraded from `RandomizedHoldout` to `MatchedQuasiExperiment`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExperimentAssignment {
    /// Unique experiment ID — links treatment and control arms.
    pub experiment_id: String,
    /// The candidate that triggered this assignment.
    pub candidate_id: String,
    /// The experimental unit being randomized — the isolatable entity.
    /// This is NOT always the workspace. It can be: audience, target/
    /// community, campaign, cohort, city, or another isolatable unit.
    pub unit_id: String,
    /// The kind of experimental unit.
    pub unit_kind: ExperimentUnitKind,
    /// Treatment or control arm.
    pub arm: TreatmentAssignment,
    /// When the assignment was made.
    pub assigned_at: OffsetDateTime,
    /// The propensity score — P(assigned to treatment). Used for IPW.
    pub propensity: f64,
    /// The template that WOULD have been dispatched (counterfactual pairing).
    pub intended_template_id: String,
    /// The dispatch context at assignment time.
    pub context: DispatchContext,
    /// The brain's prediction at assignment time.
    pub prediction: DispatchPrediction,
    /// The action_id if treatment (None for control).
    pub action_id: Option<uuid::Uuid>,
    /// Estimated contamination from concurrent actions on the same unit.
    /// 0.0 = clean, 1.0 = fully contaminated. Used to downgrade
    /// evidence quality when the control is not isolated.
    pub contamination_estimate: f64,
    /// Whether the experimental unit can reasonably remain untreated
    /// by the intervention being tested. If false, the assignment is
    /// recorded as `MatchedQuasiExperiment` regardless of randomization,
    /// because the control is not clean.
    pub is_interference_controllable: bool,
}

impl ExperimentAssignment {
    /// Returns the experiment kind, derived from interference controllability.
    /// When `is_interference_controllable` is false, the assignment is a
    /// matched quasi-experiment, not a clean randomized holdout.
    #[must_use]
    pub fn kind(&self) -> ExperimentKind {
        if self.is_interference_controllable {
            ExperimentKind::RandomizedHoldout
        } else {
            ExperimentKind::MatchedQuasiExperiment
        }
    }
}
