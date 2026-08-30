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
//!
//! # Experiment Identity (EXPERIMENT ID ≠ ACTION ID)
//!
//! Each experiment has a unique `experiment_uuid` that links all assignments
//! in the same experiment round. The `assignment_id` (stored as `id` in the
//! DB) is unique per assignment row. Multiple experiments can involve the
//! same unit over time via `assignment_round`.
//!
//! One assignment per `(experiment_uuid, assignment_round, unit_id)` — the
//! arm is a property of the assignment, not a separate row. This avoids
//! the old bug where treatment and control arms shared the same PK and
//! the second arm was silently dropped by ON CONFLICT.
//!
//! # Interference Policy (UNIT VALIDITY ≠ UNIT DECLARATION)
//!
//! An `ExperimentUnitKind` is not trusted merely because it is declared.
//! The `InterferencePolicy` determines isolatability from the actual
//! intervention type: `TargetCommunity + community.engage` may be
//! isolatable, but `TargetCommunity + global_release` is not.

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

/// The interference policy — determines whether the experimental unit
/// can actually be isolated for the intervention being tested.
///
/// This is NOT a declarative assertion. The policy is derived from the
/// interaction of unit kind and intervention type:
/// - `TargetCommunity + community.engage` → `PotentiallyIsolatable`
/// - `TargetCommunity + social_post` → `MaybeNotIsolatable`
/// - `TargetCommunity + global_release` → `NotIsolatable`
/// - `Workspace + any` → `NotIsolatable`
///
/// UNIT VALIDITY ≠ UNIT DECLARATION. A `TargetCommunity` unit is only
/// isolatable if the intervention doesn't spill across communities.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterferencePolicy {
    /// The unit can potentially remain untreated — clean randomized
    /// holdout is possible (subject to contamination scan).
    PotentiallyIsolatable,
    /// The unit may not be isolatable — contamination is likely but
    /// not certain. Downgrade to MatchedQuasiExperiment unless the
    /// contamination scan confirms the unit is clean.
    MaybeNotIsolatable,
    /// The unit cannot be isolated — the intervention spills across
    /// units. Always MatchedQuasiExperiment.
    NotIsolatable,
    /// The interference policy has not been determined. Treated as
    /// `MaybeNotIsolatable` for safety.
    Unknown,
}

impl InterferencePolicy {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PotentiallyIsolatable => "potentially_isolatable",
            Self::MaybeNotIsolatable => "maybe_not_isolatable",
            Self::NotIsolatable => "not_isolatable",
            Self::Unknown => "unknown",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "potentially_isolatable" => Some(Self::PotentiallyIsolatable),
            "maybe_not_isolatable" => Some(Self::MaybeNotIsolatable),
            "not_isolatable" => Some(Self::NotIsolatable),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }

    /// Returns true if the unit can potentially remain untreated for
    /// this intervention. `PotentiallyIsolatable` → true, everything
    /// else → false.
    #[must_use]
    pub const fn is_interference_controllable(self) -> bool {
        matches!(self, Self::PotentiallyIsolatable)
    }

    /// Determines the interference policy from the unit kind and
    /// intervention type (template_id).
    ///
    /// The template_id encodes the intervention type. Both dot-style
    /// (`community.engage`) and kebab-style (`community-engager`) are
    /// accepted:
    /// - `community.*` / `community-*` → community-level, potentially isolatable
    /// - `social.*` / `social-*` → social media, may spill across communities
    /// - `global.*` / `global-*` or `release.*` / `release-*` → all communities, not isolatable
    /// - Scanner/strategist templates → not isolatable (workspace-wide)
    #[must_use]
    pub fn from_unit_and_template(unit_kind: ExperimentUnitKind, template_id: &str) -> Self {
        match unit_kind {
            ExperimentUnitKind::Workspace => Self::NotIsolatable,
            ExperimentUnitKind::TargetCommunity => {
                if template_id.starts_with("community.") || template_id.starts_with("community-") {
                    Self::PotentiallyIsolatable
                } else if template_id.starts_with("global.")
                    || template_id.starts_with("release.")
                    || template_id.starts_with("global-")
                    || template_id.starts_with("release-")
                {
                    Self::NotIsolatable
                } else {
                    // social.post, scanner.*, strategist.* etc.
                    Self::MaybeNotIsolatable
                }
            }
            // Audience/Campaign/Cohort/City — default to maybe not isolatable
            // unless we have specific evidence the unit is clean.
            _ => Self::MaybeNotIsolatable,
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
///
/// # Identity
///
/// - `assignment_id` (stored as `id` in DB): unique per assignment row.
/// - `experiment_uuid`: links all assignments in the same experiment.
/// - `assignment_round`: distinguishes repeated experiments on the same unit.
///
/// One assignment per `(experiment_uuid, assignment_round, unit_id)` — the
/// arm is a property of the assignment.
///
/// # Estimand
///
/// The holdout estimates "effect of dispatching this selected action",
/// NOT "effect of this action in the entire world." The `eligibility_criteria`
/// and `selection_context` record what made this candidate eligible and
/// what the portfolio state was at selection time, making the estimand
/// explicit for future policy evaluation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExperimentAssignment {
    /// Unique assignment ID — unique per row. Stored as `id` in the DB.
    /// Format: `asgn:{uuid}`.
    pub assignment_id: String,
    /// The experiment UUID — links all assignments in the same experiment.
    /// Multiple rounds of the same experiment share this UUID.
    pub experiment_uuid: uuid::Uuid,
    /// The assignment round — increments across cycles for the same
    /// experiment. This enables per-round randomization without
    /// permanent hash bucketing.
    pub assignment_round: u32,
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
    /// The interference policy that determined isolatability for this
    /// assignment. Derived from the unit kind and intervention type,
    /// NOT merely declared. See `InterferencePolicy`.
    pub interference_policy: InterferencePolicy,
    /// Whether the experimental unit can reasonably remain untreated
    /// by the intervention being tested. Derived from
    /// `interference_policy.is_interference_controllable()`.
    pub is_interference_controllable: bool,
    /// What made this candidate eligible for the experiment — the
    /// estimand is "effect among eligible/selected candidates", not
    /// "effect among all opportunities." Records: portfolio selected,
    /// direct action, not scanner/strategist, etc.
    #[serde(default)]
    pub eligibility_criteria: serde_json::Value,
    /// The portfolio state at selection time — how many candidates,
    /// what was the best alternative, etc. Makes the selection-biased
    /// estimand explicit for future policy evaluation.
    #[serde(default)]
    pub selection_context: serde_json::Value,
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

    /// Constructs an `ExperimentAssignment` from an `ExperimentDesign` for a
    /// specific unit and arm. This is the canonical way to create assignments
    /// — the design defines the experiment universe, and each unit gets one
    /// assignment within that universe.
    ///
    /// The `candidate_id` is the decision key of the candidate that triggered
    /// this unit's inclusion. The `prediction` is the brain's prediction at
    /// assignment time. The `action_id` is `Some` for treatment arms (the
    /// dispatched action) and `None` for control arms.
    #[must_use]
    pub fn from_design(
        design: &ExperimentDesign,
        unit_id: &str,
        candidate_id: &str,
        arm: TreatmentAssignment,
        prediction: &crate::causal_model::DispatchPrediction,
        action_id: Option<uuid::Uuid>,
    ) -> Self {
        let assignment_uuid = uuid::Uuid::now_v7();
        Self {
            assignment_id: format!("asgn:{assignment_uuid}"),
            experiment_uuid: design.experiment_uuid,
            assignment_round: design.assignment_round,
            candidate_id: candidate_id.to_owned(),
            unit_id: unit_id.to_owned(),
            unit_kind: design.unit_kind,
            arm,
            assigned_at: design.assigned_at,
            propensity: 1.0 - design.holdout_probability,
            intended_template_id: design.intervention_key.clone(),
            context: prediction.context.clone(),
            prediction: prediction.clone(),
            action_id,
            contamination_estimate: 0.0,
            interference_policy: design.interference_policy,
            is_interference_controllable: design.interference_policy.is_interference_controllable(),
            eligibility_criteria: design.eligibility_criteria.clone(),
            selection_context: design.selection_context.clone(),
        }
    }
}

/// A first-class experiment design — defines the experiment universe BEFORE
/// any arms are assigned.
///
/// EXPERIMENT DESIGN ≠ CANDIDATE DISPATCH. The design is created FIRST, then
/// units are assigned to treatment/control arms within that design. This
/// ensures that treatment and control are genuinely arms of the same
/// experiment, not separate experiments that happen to share a label.
///
/// # Identity
///
/// One `ExperimentDesign` per (cycle, intervention). All eligible units for
/// that intervention in that cycle share the same `experiment_uuid`. The
/// `assignment_round` is 1 for each new experiment (each cycle is a new
/// experiment — we do NOT create one perpetual experiment per intervention,
/// because that would conflate different contexts, strategies, audiences,
/// and model versions).
///
/// # Cross-experiment learning
///
/// Experiments are isolated for causal integrity. Cross-experiment learning
/// happens via the hierarchical learner (calibration, treatment-effect
/// posteriors), not via shared experiment identity. The learner pools
/// evidence across experiments; the experiments themselves remain separate.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExperimentDesign {
    /// The experiment UUID — shared by all assignments in this experiment.
    /// Fresh per (cycle, intervention). All eligible units share it.
    pub experiment_uuid: uuid::Uuid,
    /// The assignment round — always 1 for a new experiment. Each cycle
    /// creates a new experiment, so rounds do not increment across cycles.
    pub assignment_round: u32,
    /// The intervention key (template_id). Different interventions in the
    /// same cycle are different experiments. Long-term this may become a
    /// richer `InterventionDefinition` so treatment-version changes create
    /// distinct experiments.
    pub intervention_key: String,
    /// The kind of experimental unit being randomized.
    pub unit_kind: ExperimentUnitKind,
    /// The eligible unit population — all units that could be assigned.
    /// Each unit gets exactly one assignment (treatment or control).
    pub eligible_units: Vec<String>,
    /// The estimand — what this experiment is estimating. Records the
    /// intervention, strategy, and selection context so the estimand is
    /// explicit: "effect among eligible/selected candidates under this
    /// strategy/context", not "effect among all opportunities."
    pub estimand: serde_json::Value,
    /// The interference policy — determines isolatability for this
    /// intervention type. Derived from unit kind + intervention key.
    pub interference_policy: InterferencePolicy,
    /// When the experiment was designed (cycle time).
    pub assigned_at: OffsetDateTime,
    /// The holdout probability — P(assigned to control). Clamped to
    /// [0.0, 0.10]. 0.0 = no holdout (all treatment). The propensity
    /// for each assignment is `1.0 - holdout_probability`.
    pub holdout_probability: f64,
    /// What made these candidates eligible for the experiment. Shared
    /// by all assignments in this experiment. Records: portfolio selected,
    /// direct action, not scanner/strategist, etc.
    pub eligibility_criteria: serde_json::Value,
    /// The portfolio state at experiment design time — how many candidates,
    /// what was the best alternative, etc. Makes the selection-biased
    /// estimand explicit for future policy evaluation.
    pub selection_context: serde_json::Value,
}

impl ExperimentDesign {
    /// Creates a new experiment design for a given intervention in a cycle.
    ///
    /// The `experiment_uuid` is fresh (caller generates it). The
    /// `assignment_round` is 1 (each cycle is a new experiment). The
    /// `interference_policy` is derived from the unit kind and intervention
    /// key.
    #[must_use]
    pub fn new(
        experiment_uuid: uuid::Uuid,
        intervention_key: &str,
        unit_kind: ExperimentUnitKind,
        eligible_units: Vec<String>,
        assigned_at: OffsetDateTime,
        holdout_probability: f64,
        strategy: &str,
    ) -> Self {
        let interference_policy =
            InterferencePolicy::from_unit_and_template(unit_kind, intervention_key);
        let estimand = serde_json::json!({
            "intervention": intervention_key,
            "strategy": strategy,
            "unit_kind": unit_kind.as_str(),
            "population_size": eligible_units.len(),
        });
        let eligibility_criteria = serde_json::json!({
            "is_direct_action": true,
            "template_id": intervention_key,
            "portfolio_selected": true,
        });
        let selection_context = serde_json::json!({
            "holdout_probability": holdout_probability,
            "strategy": strategy,
        });
        Self {
            experiment_uuid,
            assignment_round: 1,
            intervention_key: intervention_key.to_owned(),
            unit_kind,
            eligible_units,
            estimand,
            interference_policy,
            assigned_at,
            holdout_probability,
            eligibility_criteria,
            selection_context,
        }
    }
}

/// A fan provenance event — an append-only record of a fan's exposure,
/// interaction, conversion, or durability milestone.
///
/// PROVENANCE ≠ CAUSALITY. These events establish exposure/attribution
/// evidence. They do NOT automatically establish causal treatment effect.
/// The semantic layers are kept separate:
///   EXPOSURE → ATTRIBUTION → CAUSAL ESTIMATE
///
/// Event semantics:
/// - `Exposure` — fan was exposed to an action (post seen, email received)
/// - `Interaction` — fan engaged (click, reply, share)
/// - `Conversion` — fan signed up / became a fan
/// - `Durability` — fan still active after 30 days
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FanProvenanceEvent {
    /// The fan, if known. Nullable for anonymous exposure events.
    pub fan_id: Option<uuid::Uuid>,
    /// The kind of provenance event.
    pub event_kind: ProvenanceEventKind,
    /// The channel (reddit, instagram, email, etc.).
    pub channel: String,
    /// The source target (e.g. subreddit name).
    pub source_target: Option<String>,
    /// The community (e.g. r/djent).
    pub community: Option<String>,
    /// The campaign ID, if applicable.
    pub campaign_id: Option<uuid::Uuid>,
    /// The action ID that triggered this event, if known.
    pub action_id: Option<uuid::Uuid>,
    /// The attribution method (tracked_link, temporal, unknown, etc.).
    pub attribution_method: String,
    /// Attribution confidence (0.0–1.0).
    pub attribution_confidence: f64,
    /// When the event occurred.
    pub occurred_at: OffsetDateTime,
}

/// The kind of fan provenance event.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceEventKind {
    /// Fan was exposed to an action (post seen, email received).
    Exposure,
    /// Fan engaged meaningfully (click, reply, share).
    Interaction,
    /// Fan signed up / became a fan.
    Conversion,
    /// Fan remained active after 30 days.
    Durability,
}

impl ProvenanceEventKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exposure => "exposure",
            Self::Interaction => "interaction",
            Self::Conversion => "conversion",
            Self::Durability => "durability",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "exposure" => Some(Self::Exposure),
            "interaction" => Some(Self::Interaction),
            "conversion" => Some(Self::Conversion),
            "durability" => Some(Self::Durability),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interference_policy_community_engage_is_isolatable() {
        let policy = InterferencePolicy::from_unit_and_template(
            ExperimentUnitKind::TargetCommunity,
            "community.engage",
        );
        assert_eq!(policy, InterferencePolicy::PotentiallyIsolatable);
        assert!(policy.is_interference_controllable());
    }

    #[test]
    fn interference_policy_global_release_is_not_isolatable() {
        let policy = InterferencePolicy::from_unit_and_template(
            ExperimentUnitKind::TargetCommunity,
            "global.release_campaign",
        );
        assert_eq!(policy, InterferencePolicy::NotIsolatable);
        assert!(!policy.is_interference_controllable());
    }

    #[test]
    fn interference_policy_social_post_is_maybe_not_isolatable() {
        let policy = InterferencePolicy::from_unit_and_template(
            ExperimentUnitKind::TargetCommunity,
            "social.post",
        );
        assert_eq!(policy, InterferencePolicy::MaybeNotIsolatable);
        assert!(!policy.is_interference_controllable());
    }

    #[test]
    fn interference_policy_workspace_is_never_isolatable() {
        for template in &["community.engage", "social.post", "global.release"] {
            let policy =
                InterferencePolicy::from_unit_and_template(ExperimentUnitKind::Workspace, template);
            assert_eq!(policy, InterferencePolicy::NotIsolatable);
        }
    }

    #[test]
    fn experiment_kind_from_interference() {
        let policy = InterferencePolicy::PotentiallyIsolatable;
        assert!(policy.is_interference_controllable());
        // RandomizedHoldout when controllable
    }

    #[test]
    fn interference_policy_round_trips() {
        for policy in [
            InterferencePolicy::PotentiallyIsolatable,
            InterferencePolicy::MaybeNotIsolatable,
            InterferencePolicy::NotIsolatable,
            InterferencePolicy::Unknown,
        ] {
            assert_eq!(InterferencePolicy::parse(policy.as_str()), Some(policy));
        }
    }

    // ── ExperimentDesign tests ──

    fn make_prediction(template: &str) -> crate::causal_model::DispatchPrediction {
        crate::causal_model::DispatchPrediction {
            template_id: template.to_owned(),
            expected_new_fans: 5.0,
            expected_signal_installs: 1.0,
            context: crate::causal_model::DispatchContext::default(),
        }
    }

    #[test]
    fn experiment_design_creates_with_correct_identity() {
        let uuid = uuid::Uuid::now_v7();
        let design = ExperimentDesign::new(
            uuid,
            "community.engage",
            ExperimentUnitKind::TargetCommunity,
            vec!["r/djent".to_owned(), "r/metalcore".to_owned()],
            OffsetDateTime::now_utc(),
            0.05,
            "discovery",
        );
        assert_eq!(design.experiment_uuid, uuid);
        assert_eq!(design.assignment_round, 1);
        assert_eq!(design.intervention_key, "community.engage");
        assert_eq!(design.eligible_units.len(), 2);
        assert_eq!(
            design.interference_policy,
            InterferencePolicy::PotentiallyIsolatable
        );
        assert!((design.holdout_probability - 0.05).abs() < 1e-10);
    }

    #[test]
    fn from_design_shares_experiment_uuid_across_arms() {
        let uuid = uuid::Uuid::now_v7();
        let design = ExperimentDesign::new(
            uuid,
            "community.engage",
            ExperimentUnitKind::TargetCommunity,
            vec!["r/djent".to_owned(), "r/metalcore".to_owned()],
            OffsetDateTime::now_utc(),
            0.05,
            "discovery",
        );
        let pred = make_prediction("community.engage");
        let treatment = ExperimentAssignment::from_design(
            &design,
            "r/djent",
            "r/djent",
            TreatmentAssignment::Treatment,
            &pred,
            Some(uuid::Uuid::now_v7()),
        );
        let control = ExperimentAssignment::from_design(
            &design,
            "r/metalcore",
            "r/metalcore",
            TreatmentAssignment::Control,
            &pred,
            None,
        );
        // Both assignments share the same experiment_uuid and round.
        assert_eq!(treatment.experiment_uuid, control.experiment_uuid);
        assert_eq!(treatment.experiment_uuid, uuid);
        assert_eq!(treatment.assignment_round, control.assignment_round);
        assert_eq!(treatment.assignment_round, 1);
        // Different units, different arms.
        assert_ne!(treatment.unit_id, control.unit_id);
        assert_ne!(treatment.arm, control.arm);
        // Same interference policy (derived from the same intervention).
        assert_eq!(treatment.interference_policy, control.interference_policy);
        // Propensity is the same (1 - holdout_probability).
        assert!((treatment.propensity - control.propensity).abs() < 1e-10);
        assert!((treatment.propensity - 0.95).abs() < 1e-10);
    }

    #[test]
    fn from_design_assignment_ids_are_unique() {
        let uuid = uuid::Uuid::now_v7();
        let design = ExperimentDesign::new(
            uuid,
            "community.engage",
            ExperimentUnitKind::TargetCommunity,
            vec!["r/djent".to_owned()],
            OffsetDateTime::now_utc(),
            0.05,
            "discovery",
        );
        let pred = make_prediction("community.engage");
        let a1 = ExperimentAssignment::from_design(
            &design,
            "r/djent",
            "r/djent",
            TreatmentAssignment::Treatment,
            &pred,
            None,
        );
        let a2 = ExperimentAssignment::from_design(
            &design,
            "r/djent",
            "r/djent",
            TreatmentAssignment::Treatment,
            &pred,
            None,
        );
        assert_ne!(a1.assignment_id, a2.assignment_id);
    }
}
