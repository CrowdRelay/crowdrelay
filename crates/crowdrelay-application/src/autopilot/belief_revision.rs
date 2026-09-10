//! What a cycle's evidence changed in the brain's beliefs.
//!
//! The learning loop closes in two places — the strategy posterior, updated
//! from resolved growth evidence during the causal model load, and the
//! hypothesis lifecycle, which degrades a template that fails walk-forward
//! validation. Both are read back by the next cycle, and neither left a
//! record of *why* it moved: `viryaos_brain_state` and
//! `viryaos_growth_hypotheses` are updated in place.
//!
//! A [`BeliefRevision`] is that record. It names the belief, what it was,
//! what it became, and the evidence rows that moved it — which carry the
//! action, and therefore the decision and the cycle that produced them.
//!
//! Nothing in the brain reads these. A revision is written after the belief
//! it describes has already been saved, so a failed write costs the operator
//! the explanation and never costs the brain the learning.

use serde::Serialize;
use uuid::Uuid;

/// The belief that moved.
///
/// Narrow on purpose: every variant has to be able to say what its
/// `belief_key` identifies, because a ledger nobody can join is a log.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BeliefModule {
    /// The state-conditioned strategy posterior. `belief_key` is the cell
    /// key, `strategy:growth_trend:event_proximity`.
    StrategyPosterior,
    /// The hypothesis lifecycle state for a worker template. `belief_key` is
    /// the template_id.
    HypothesisState,
}

impl BeliefModule {
    /// The value stored in the `module` column. Must match the CHECK
    /// constraint in `0252_brain_belief_revisions.sql`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StrategyPosterior => "strategy_posterior",
            Self::HypothesisState => "hypothesis_state",
        }
    }
}

/// One belief that moved, and the evidence that moved it.
#[derive(Clone, Debug, Serialize)]
pub struct BeliefRevision {
    pub module: BeliefModule,
    pub belief_key: String,
    /// The belief before and after, as the owning module serializes it.
    pub previous_value: serde_json::Value,
    pub current_value: serde_json::Value,
    /// One line an operator can read without decoding the values.
    pub change_summary: String,
    /// The actions whose measured outcomes moved this belief.
    ///
    /// Actions rather than the evidence rows themselves: an action carries the
    /// decision that authorized it, and the decision carries the cycle and the
    /// trace, so this is the end of the chain an operator can actually walk.
    /// Empty is not legitimate — a revision that cannot name its cause is not
    /// proof of learning — and writers drop such a revision rather than
    /// record it.
    pub caused_by_action_ids: Vec<Uuid>,
}

impl BeliefRevision {
    /// Whether this revision can support the claim it exists to make.
    ///
    /// A revision with no cited evidence says a belief changed and cannot say
    /// what changed it, which is exactly the gap the ledger was added to
    /// close. Writers check this rather than persisting an unattributable row.
    #[must_use]
    pub fn is_attributable(&self) -> bool {
        !self.caused_by_action_ids.is_empty()
    }
}

/// How far a posterior cell's mean has to move before the change is worth
/// recording, in expected incremental fans.
///
/// A Normal-Normal update moves every cell it touches by some amount, and
/// most of those movements are the fourth decimal place of a belief nobody
/// would act on differently. Recording them would bury the revisions that
/// changed a decision under thousands that changed nothing. A tenth of a fan
/// is the smallest movement that can plausibly reorder two strategies, which
/// is the only thing the posterior is consulted for.
const MATERIAL_MEAN_SHIFT: f64 = 0.1;

/// The revisions between two states of the strategy posterior.
///
/// Compares by cell. A cell that appears for the first time is a revision
/// from the skeptical prior, which is a real change in what the brain
/// believes about that (strategy, state) pair — before it, the pair had no
/// evidence at all. A cell whose mean moved less than
/// [`MATERIAL_MEAN_SHIFT`] is not recorded: see that constant for why.
///
/// `caused_by_action_ids` is the same list for every revision this call
/// produces, because the replay applies one batch of resolved outcomes to
/// every cell at once and nothing in the update records which observation
/// landed on which cell. Attributing a subset would be a guess.
#[must_use]
pub fn strategy_posterior_revisions(
    before: &crowdrelay_brain::StateConditionedStrategyPosterior,
    after: &crowdrelay_brain::StateConditionedStrategyPosterior,
    caused_by_action_ids: &[Uuid],
) -> Vec<BeliefRevision> {
    use std::collections::HashMap;

    if caused_by_action_ids.is_empty() {
        // Nothing to attribute the change to. See `is_attributable`.
        return Vec::new();
    }
    let previous: HashMap<&str, (f64, f64, u32)> = before
        .cells()
        .map(|(key, mean, variance, n)| (key, (mean, variance, n)))
        .collect();

    after
        .cells()
        .filter_map(|(key, mean, variance, observations)| {
            let prior = previous.get(key).copied();
            let previous_mean = prior.map(|(mean, _, _)| mean);
            if let Some(previous_mean) = previous_mean
                && (mean - previous_mean).abs() < MATERIAL_MEAN_SHIFT
            {
                return None;
            }
            let change_summary = match previous_mean {
                Some(previous_mean) => format!(
                    "expected incremental fans for {key} moved {previous_mean:.2} → {mean:.2} \
                     over {observations} observations"
                ),
                None => format!(
                    "first evidence for {key}: expected incremental fans {mean:.2} \
                     over {observations} observations"
                ),
            };
            Some(BeliefRevision {
                module: BeliefModule::StrategyPosterior,
                belief_key: key.to_owned(),
                previous_value: prior.map_or_else(
                    || serde_json::json!({ "observations": 0 }),
                    |(mean, variance, observations)| {
                        serde_json::json!({
                            "mean": mean,
                            "variance": variance,
                            "observations": observations,
                        })
                    },
                ),
                current_value: serde_json::json!({
                    "mean": mean,
                    "variance": variance,
                    "observations": observations,
                }),
                change_summary,
                caused_by_action_ids: caused_by_action_ids.to_vec(),
            })
        })
        .collect()
}

/// The revision for a hypothesis lifecycle transition.
///
/// Returns `None` when the state did not change, or when no action can be
/// named as the cause — a lifecycle transition whose evidence cannot be
/// pointed at is not proof of anything.
#[must_use]
pub fn hypothesis_state_revision(
    template_id: &str,
    previous: crowdrelay_brain::hypothesis::HypothesisState,
    current: crowdrelay_brain::hypothesis::HypothesisState,
    out_of_sample_observations: u32,
    caused_by_action_ids: &[Uuid],
) -> Option<BeliefRevision> {
    if previous == current || caused_by_action_ids.is_empty() {
        return None;
    }
    Some(BeliefRevision {
        module: BeliefModule::HypothesisState,
        belief_key: template_id.to_owned(),
        previous_value: serde_json::json!({ "state": previous.as_str() }),
        current_value: serde_json::json!({ "state": current.as_str() }),
        change_summary: format!(
            "{template_id} moved {} → {} after failing walk-forward validation \
             on {out_of_sample_observations} out-of-sample observations",
            previous.as_str(),
            current.as_str(),
        ),
        caused_by_action_ids: caused_by_action_ids.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crowdrelay_brain::StateConditionedStrategyPosterior;

    fn action() -> Vec<Uuid> {
        vec![Uuid::from_u128(1)]
    }

    #[test]
    fn a_new_cell_is_recorded_as_a_revision_from_no_evidence() {
        let before = StateConditionedStrategyPosterior::new();
        let mut after = before.clone();
        for _ in 0..5 {
            after.update("community_first", "steady", "far", 8.0, 4.0);
        }

        let revisions = strategy_posterior_revisions(&before, &after, &action());

        assert_eq!(revisions.len(), 1, "one cell moved");
        let revision = &revisions[0];
        assert_eq!(revision.belief_key, "community_first:steady:far");
        assert_eq!(revision.previous_value["observations"], 0);
        assert!(revision.is_attributable());
    }

    #[test]
    fn an_immaterial_shift_is_not_recorded() {
        let mut before = StateConditionedStrategyPosterior::new();
        for _ in 0..50 {
            before.update("community_first", "steady", "far", 8.0, 4.0);
        }
        let mut after = before.clone();
        // One more observation at the mean the cell already holds moves it by
        // far less than a tenth of a fan.
        after.update("community_first", "steady", "far", 8.0, 4.0);

        assert!(strategy_posterior_revisions(&before, &after, &action()).is_empty());
    }

    #[test]
    fn an_unattributable_change_is_not_recorded() {
        let before = StateConditionedStrategyPosterior::new();
        let mut after = before.clone();
        after.update("community_first", "steady", "far", 8.0, 4.0);

        assert!(
            strategy_posterior_revisions(&before, &after, &[]).is_empty(),
            "a revision that cannot name what moved it is not proof of learning"
        );
    }

    #[test]
    fn an_unchanged_hypothesis_state_is_not_a_revision() {
        use crowdrelay_brain::hypothesis::HypothesisState;

        assert!(
            hypothesis_state_revision(
                "community-engager",
                HypothesisState::Active,
                HypothesisState::Active,
                12,
                &action(),
            )
            .is_none()
        );
    }
}
