//! Hierarchical planning with options — temporally extended actions.
//!
//! The brain doesn't just dispatch isolated actions; it plans *options* —
//! multi-step action sequences that it can execute as a unit. An option is a
//! temporally extended action: "scan Reddit for metal communities" →
//! "engage the top 3 communities" → "invite engaged fans to Signal".
//!
//! # Why options?
//!
//! Single-action dispatch is myopic. The brain needs to reason about
//! *sequences* of actions that build on each other:
//!
//! 1. **Scan** a community to find where fans congregate.
//! 2. **Engage** those communities to build genuine interest.
//! 3. **Invite** the engaged fans to install Signal.
//!
//! Each step depends on the output of the previous one. By packaging these
//! into an option, the brain can:
//!
//! - Track the full plan as a unit (not lose the thread between cycles).
//! - Model dependencies between steps (step 3 needs step 1 and 2 to succeed).
//! - Estimate the total expected fan yield of the sequence.
//! - Abandon the whole option if a critical step fails.
//!
//! # Step dependencies
//!
//! Steps within an option can have dependencies: step *i* can declare that it
//! `depends_on` steps *j*, *k*, … (by index). A step is `Ready` only when all
//! of its dependencies are `Completed`. Steps with no dependencies are ready
//! immediately. This allows both sequential pipelines
//! (1 → 2 → 3) and parallel fan-out (1 → {2, 3} → 4).
//!
//! # Overlap penalty
//!
//! When steps target overlapping audiences, the total expected fans is *not*
//! the naive sum — some fans would have been captured by multiple steps. The
//! `total_expected_fans` field applies a simple overlap penalty so the brain
//! doesn't double-count.

use serde::Serialize;

/// The overlap penalty factor applied when summing step expected fans.
///
/// Each additional step beyond the first contributes only
/// `OVERLAP_PENALTY_FACTOR` of its expected fans, modelling that later steps
/// in a sequence partially re-target the same audience as earlier ones.
pub const OVERLAP_PENALTY_FACTOR: f64 = 0.85;

/// The lifecycle status of an entire option.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionStatus {
    /// The option has been created but no step has been dispatched yet.
    #[default]
    Pending,
    /// At least one step has been dispatched and the option is executing.
    InProgress,
    /// All steps have completed successfully.
    Completed,
    /// A critical step failed and the option was abandoned.
    Abandoned,
}

/// The lifecycle status of a single step within an option.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionStepStatus {
    /// The step has not been dispatched and its dependencies are not yet met.
    #[default]
    Pending,
    /// All dependencies have completed; the step is ready to be dispatched.
    Ready,
    /// The step has been dispatched to a worker and is executing.
    Dispatched,
    /// The step completed successfully.
    Completed,
    /// The step failed.
    Failed,
}

/// A single step within an option — one worker dispatch in the sequence.
#[derive(Clone, Debug, Serialize)]
pub struct OptionStep {
    /// The worker template to dispatch for this step (e.g. "reddit-scanner").
    pub template_id: String,
    /// The target for this step (e.g. "r_MetalMusic", "signal-invite-link").
    pub target: String,
    /// The expected number of fans from this step *alone*.
    pub expected_fans: f64,
    /// Indices of steps (within the option's `steps` vector) that must
    /// complete before this step can be dispatched.
    pub depends_on: Vec<usize>,
    /// The current lifecycle status of this step.
    pub status: OptionStepStatus,
}

impl OptionStep {
    /// Creates a new step with the given template, target, expected fans, and
    /// dependencies. The step starts in `Pending` status.
    #[must_use]
    pub fn new(
        template_id: &str,
        target: &str,
        expected_fans: f64,
        depends_on: Vec<usize>,
    ) -> Self {
        Self {
            template_id: template_id.to_owned(),
            target: target.to_owned(),
            expected_fans,
            depends_on,
            status: OptionStepStatus::Pending,
        }
    }

    /// Returns `true` if this step has no dependencies and can be dispatched
    /// immediately.
    #[must_use]
    pub fn has_no_dependencies(&self) -> bool {
        self.depends_on.is_empty()
    }
}

/// An option — a temporally extended, multi-step action sequence.
///
/// # Example: the "Reddit → Signal" pipeline
///
/// ```text
/// step 0: scan r/MetalMusic        (no deps)
/// step 1: engage r/MetalMusic      (depends on 0)
/// step 2: invite to Signal         (depends on 1)
/// ```
///
/// The brain starts the option, dispatches step 0, and on each step completion
/// calls [`OptionPlanner::advance`] to mark the step done and get the next
/// ready step.
#[derive(Clone, Debug, Serialize)]
pub struct ActionOption {
    /// A stable identifier for this option (used to track it across cycles).
    pub id: String,
    /// A human-readable name (e.g. "Reddit metal pipeline").
    pub name: String,
    /// The ordered sequence of steps that make up this option.
    pub steps: Vec<OptionStep>,
    /// The total expected fans from the whole option, with overlap penalty.
    pub total_expected_fans: f64,
    /// The current lifecycle status of the option.
    pub status: OptionStatus,
    /// When the option was started (first step dispatched), if ever.
    pub started_at: Option<time::OffsetDateTime>,
    /// When the option reached a terminal state, if ever.
    pub completed_at: Option<time::OffsetDateTime>,
}

impl ActionOption {
    /// Creates a new option with the given id, name, and steps.
    ///
    /// `total_expected_fans` is computed from the steps using an overlap
    /// penalty: the first step contributes its full `expected_fans`, and each
    /// subsequent step contributes `OVERLAP_PENALTY_FACTOR` of its
    /// `expected_fans`, modelling that later steps partially re-target the
    /// same audience.
    ///
    /// Steps with no dependencies are immediately marked `Ready`; the rest
    /// stay `Pending` until their dependencies complete.
    #[must_use]
    pub fn new(id: &str, name: &str, steps: Vec<OptionStep>) -> Self {
        let total_expected_fans = compute_total_expected_fans(&steps);
        let mut option = Self {
            id: id.to_owned(),
            name: name.to_owned(),
            steps,
            total_expected_fans,
            status: OptionStatus::Pending,
            started_at: None,
            completed_at: None,
        };
        option.mark_ready_steps();
        option
    }

    /// Marks every `Pending` step whose dependencies are all `Completed` as
    /// `Ready`. This is called after a step completes.
    fn mark_ready_steps(&mut self) {
        // First, collect the indices of steps that should be promoted. We
        // can't mutate `self.steps` while iterating over it and checking
        // dependencies (which requires reading other steps), so we gather
        // the decisions first.
        let to_promote: Vec<usize> = self
            .steps
            .iter()
            .enumerate()
            .filter(|(_, step)| step.status == OptionStepStatus::Pending)
            .filter(|(_, step)| {
                step.depends_on.iter().all(|&dep| {
                    self.steps
                        .get(dep)
                        .is_some_and(|s| s.status == OptionStepStatus::Completed)
                })
            })
            .map(|(idx, _)| idx)
            .collect();
        for idx in to_promote {
            self.steps[idx].status = OptionStepStatus::Ready;
        }
    }

    /// Returns `true` if every step in the option is `Completed`.
    fn all_steps_completed(&self) -> bool {
        self.steps
            .iter()
            .all(|s| s.status == OptionStepStatus::Completed)
    }

    /// Returns `true` if any step in the option is `Failed`.
    fn any_step_failed(&self) -> bool {
        self.steps
            .iter()
            .any(|s| s.status == OptionStepStatus::Failed)
    }

    /// Returns the index of the first `Ready` step, if any.
    fn first_ready_step(&self) -> Option<usize> {
        self.steps
            .iter()
            .position(|s| s.status == OptionStepStatus::Ready)
    }
}

/// Computes the total expected fans for an option from its steps, applying an
/// overlap penalty so that later steps contribute less (they partially
/// re-target the same audience as earlier steps).
///
/// The first step contributes its full `expected_fans`; each subsequent step
/// contributes `OVERLAP_PENALTY_FACTOR` of its `expected_fans`.
#[must_use]
pub fn compute_total_expected_fans(steps: &[OptionStep]) -> f64 {
    let mut total = 0.0;
    for (i, step) in steps.iter().enumerate() {
        if i == 0 {
            total += step.expected_fans;
        } else {
            total += step.expected_fans * OVERLAP_PENALTY_FACTOR;
        }
    }
    total
}

/// The option planner — tracks all active and completed options and advances
/// them as steps complete.
///
/// The brain creates an [`ActionOption`], passes it to the planner via
/// [`OptionPlanner::start_option`], and then calls
/// [`OptionPlanner::advance`] each time a step's result comes back. The
/// planner handles dependency resolution, status transitions, and returns the
/// next ready step to dispatch.
#[derive(Clone, Debug, Default, Serialize)]
pub struct OptionPlanner {
    /// All options known to the planner (active + completed + abandoned).
    pub active_options: Vec<ActionOption>,
}

impl OptionPlanner {
    /// Creates a new, empty option planner.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts tracking an option. The option's status transitions to
    /// `InProgress` and its `started_at` is set to the current time. Any steps
    /// that were already `Ready` (no dependencies) are available for
    /// immediate dispatch via [`Self::advance`] (called with a dummy success)
    /// or by inspecting the option directly.
    pub fn start_option(&mut self, mut option: ActionOption) {
        option.status = OptionStatus::InProgress;
        option.started_at = Some(time::OffsetDateTime::now_utc());
        self.active_options.push(option);
    }

    /// Advances the option identified by `action_id` after a step result.
    ///
    /// # What it does
    ///
    /// 1. Finds the option with the given `action_id`.
    /// 2. Finds the step that is currently `Dispatched` (the one whose result
    ///    just came back) and marks it `Completed` (if `success`) or `Failed`
    ///    (if not).
    /// 3. If `success`: marks any `Pending` steps whose dependencies are now
    ///    met as `Ready`.
    /// 4. If all steps are `Completed`: marks the option `Completed` and sets
    ///    `completed_at`.
    /// 5. If a step failed: marks the option `Abandoned` and sets
    ///    `completed_at`. (Any step failure is treated as critical.)
    /// 6. Returns a reference to the next `Ready` step to dispatch, if any.
    ///
    /// Returns `None` if the option was not found, is already terminal
    /// (Completed/Abandoned), all steps are completed, or no step is ready
    /// after advancing. When called with no currently dispatched step but a
    /// `Ready` step exists, the ready step is returned unchanged (the call is
    /// a no-op aside from the lookup).
    #[must_use]
    pub fn advance(&mut self, action_id: &str, success: bool) -> Option<&OptionStep> {
        let option = self.active_options.iter_mut().find(|o| o.id == action_id)?;
        if option.status == OptionStatus::Completed || option.status == OptionStatus::Abandoned {
            return None;
        }

        // Find the step whose result just came back (the dispatched one).
        let dispatched_idx = option
            .steps
            .iter()
            .position(|s| s.status == OptionStepStatus::Dispatched);

        if let Some(idx) = dispatched_idx {
            let step = &mut option.steps[idx];
            step.status = if success {
                OptionStepStatus::Completed
            } else {
                OptionStepStatus::Failed
            };
        }

        if success {
            // Promote pending steps whose dependencies are now satisfied.
            option.mark_ready_steps();
        }

        // Check terminal conditions for the option.
        if option.all_steps_completed() {
            option.status = OptionStatus::Completed;
            option.completed_at = Some(time::OffsetDateTime::now_utc());
            return None;
        }
        if option.any_step_failed() {
            option.status = OptionStatus::Abandoned;
            option.completed_at = Some(time::OffsetDateTime::now_utc());
            return None;
        }

        // Return the next ready step to dispatch.
        option.first_ready_step().map(|idx| &option.steps[idx])
    }

    /// Returns the number of options currently in `InProgress` state.
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.active_options
            .iter()
            .filter(|o| o.status == OptionStatus::InProgress)
            .count()
    }

    /// Returns the number of options in a terminal state (`Completed` or
    /// `Abandoned`).
    #[must_use]
    pub fn completed_count(&self) -> usize {
        self.active_options
            .iter()
            .filter(|o| o.status == OptionStatus::Completed || o.status == OptionStatus::Abandoned)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a simple 3-step sequential option:
    /// scan → engage → invite.
    fn sequential_option() -> ActionOption {
        ActionOption::new(
            "reddit-metal-pipeline",
            "Reddit metal pipeline",
            vec![
                OptionStep::new("reddit-scanner", "r_MetalMusic", 50.0, vec![]),
                OptionStep::new("community-engager", "r_MetalMusic", 30.0, vec![0]),
                OptionStep::new("signal-inviter", "r_MetalMusic", 15.0, vec![1]),
            ],
        )
    }

    #[test]
    fn option_creation_marks_dependency_free_steps_ready() {
        let option = sequential_option();
        assert_eq!(option.id, "reddit-metal-pipeline");
        assert_eq!(option.name, "Reddit metal pipeline");
        assert_eq!(option.steps.len(), 3);
        assert_eq!(option.status, OptionStatus::Pending);

        // Step 0 has no dependencies → Ready.
        assert_eq!(option.steps[0].status, OptionStepStatus::Ready);
        // Steps 1 and 2 have unmet dependencies → Pending.
        assert_eq!(option.steps[1].status, OptionStepStatus::Pending);
        assert_eq!(option.steps[2].status, OptionStepStatus::Pending);
    }

    #[test]
    fn total_expected_fans_applies_overlap_penalty() {
        let option = sequential_option();
        // step 0: 50.0 (full)
        // step 1: 30.0 * 0.85 = 25.5
        // step 2: 15.0 * 0.85 = 12.75
        let expected = 50.0 + 30.0 * OVERLAP_PENALTY_FACTOR + 15.0 * OVERLAP_PENALTY_FACTOR;
        assert!(
            (option.total_expected_fans - expected).abs() < 0.001,
            "got {}, expected {}",
            option.total_expected_fans,
            expected
        );
    }

    #[test]
    fn total_expected_fans_single_step() {
        let option = ActionOption::new(
            "single",
            "Single",
            vec![OptionStep::new(
                "reddit-scanner",
                "r_MetalMusic",
                42.0,
                vec![],
            )],
        );
        assert!((option.total_expected_fans - 42.0).abs() < 0.001);
    }

    #[test]
    fn total_expected_fans_empty() {
        let option = ActionOption::new("empty", "Empty", vec![]);
        assert_eq!(option.total_expected_fans, 0.0);
    }

    #[test]
    fn start_option_sets_in_progress_and_started_at() {
        let mut planner = OptionPlanner::new();
        let option = sequential_option();
        planner.start_option(option);
        assert_eq!(planner.active_count(), 1);
        assert_eq!(planner.completed_count(), 0);
        let stored = &planner.active_options[0];
        assert_eq!(stored.status, OptionStatus::InProgress);
        assert!(stored.started_at.is_some());
    }

    #[test]
    fn step_dependency_resolution_promotes_pending_to_ready() {
        let mut planner = OptionPlanner::new();
        planner.start_option(sequential_option());

        // The first ready step is step 0 (scan).
        let step0 = planner
            .advance("reddit-metal-pipeline", true)
            .expect("should return next ready step");
        // Wait — advance marks the *dispatched* step. But nothing is dispatched
        // yet. We simulate dispatch by manually setting step 0 to Dispatched,
        // then calling advance.
        let _ = step0;

        // Manually mark step 0 as dispatched (simulating the brain picking it
        // up).
        let option = planner
            .active_options
            .iter_mut()
            .find(|o| o.id == "reddit-metal-pipeline")
            .unwrap();
        option.steps[0].status = OptionStepStatus::Dispatched;

        // Step 0 succeeds → step 1 should become Ready.
        let next = planner
            .advance("reddit-metal-pipeline", true)
            .expect("step 1 should be ready after step 0 completes");
        assert_eq!(next.template_id, "community-engager");
        assert_eq!(next.status, OptionStepStatus::Ready);

        let option = planner
            .active_options
            .iter()
            .find(|o| o.id == "reddit-metal-pipeline")
            .unwrap();
        assert_eq!(option.steps[0].status, OptionStepStatus::Completed);
        assert_eq!(option.steps[1].status, OptionStepStatus::Ready);
        assert_eq!(option.steps[2].status, OptionStepStatus::Pending);
    }

    #[test]
    fn sequential_execution_completes_all_steps() {
        let mut planner = OptionPlanner::new();
        planner.start_option(sequential_option());

        // Step 0: dispatch then complete.
        {
            let option = planner
                .active_options
                .iter_mut()
                .find(|o| o.id == "reddit-metal-pipeline")
                .unwrap();
            option.steps[0].status = OptionStepStatus::Dispatched;
        }
        let _ = planner.advance("reddit-metal-pipeline", true);

        // Step 1: dispatch then complete.
        {
            let option = planner
                .active_options
                .iter_mut()
                .find(|o| o.id == "reddit-metal-pipeline")
                .unwrap();
            option.steps[1].status = OptionStepStatus::Dispatched;
        }
        let _ = planner.advance("reddit-metal-pipeline", true);

        // Step 2: dispatch then complete.
        {
            let option = planner
                .active_options
                .iter_mut()
                .find(|o| o.id == "reddit-metal-pipeline")
                .unwrap();
            option.steps[2].status = OptionStepStatus::Dispatched;
        }
        let next = planner.advance("reddit-metal-pipeline", true);

        // All steps done → option completed, no next step.
        assert!(next.is_none());
        let option = planner
            .active_options
            .iter()
            .find(|o| o.id == "reddit-metal-pipeline")
            .unwrap();
        assert_eq!(option.status, OptionStatus::Completed);
        assert!(option.completed_at.is_some());
        assert!(
            option
                .steps
                .iter()
                .all(|s| s.status == OptionStepStatus::Completed)
        );
        assert_eq!(planner.active_count(), 0);
        assert_eq!(planner.completed_count(), 1);
    }

    #[test]
    fn failure_handling_abandons_option() {
        let mut planner = OptionPlanner::new();
        planner.start_option(sequential_option());

        // Step 0: dispatch then fail.
        {
            let option = planner
                .active_options
                .iter_mut()
                .find(|o| o.id == "reddit-metal-pipeline")
                .unwrap();
            option.steps[0].status = OptionStepStatus::Dispatched;
        }
        let next = planner.advance("reddit-metal-pipeline", false);

        // Step 0 failed → option abandoned, no next step.
        assert!(next.is_none());
        let option = planner
            .active_options
            .iter()
            .find(|o| o.id == "reddit-metal-pipeline")
            .unwrap();
        assert_eq!(option.status, OptionStatus::Abandoned);
        assert!(option.completed_at.is_some());
        assert_eq!(option.steps[0].status, OptionStepStatus::Failed);
        // Steps 1 and 2 remain pending (never promoted).
        assert_eq!(option.steps[1].status, OptionStepStatus::Pending);
        assert_eq!(option.steps[2].status, OptionStepStatus::Pending);
        assert_eq!(planner.active_count(), 0);
        assert_eq!(planner.completed_count(), 1);
    }

    #[test]
    fn failure_on_later_step_abandons_option() {
        let mut planner = OptionPlanner::new();
        planner.start_option(sequential_option());

        // Step 0 succeeds.
        {
            let option = planner
                .active_options
                .iter_mut()
                .find(|o| o.id == "reddit-metal-pipeline")
                .unwrap();
            option.steps[0].status = OptionStepStatus::Dispatched;
        }
        let _ = planner.advance("reddit-metal-pipeline", true);

        // Step 1 fails.
        {
            let option = planner
                .active_options
                .iter_mut()
                .find(|o| o.id == "reddit-metal-pipeline")
                .unwrap();
            option.steps[1].status = OptionStepStatus::Dispatched;
        }
        let next = planner.advance("reddit-metal-pipeline", false);

        assert!(next.is_none());
        let option = planner
            .active_options
            .iter()
            .find(|o| o.id == "reddit-metal-pipeline")
            .unwrap();
        assert_eq!(option.status, OptionStatus::Abandoned);
        assert_eq!(option.steps[0].status, OptionStepStatus::Completed);
        assert_eq!(option.steps[1].status, OptionStepStatus::Failed);
        // Step 2 depends on step 1, which failed (not Completed), so it was
        // never promoted and remains Pending.
        assert_eq!(option.steps[2].status, OptionStepStatus::Pending);
    }

    #[test]
    fn parallel_steps_all_ready_immediately() {
        // Two independent scan steps (no deps) + a join step that depends on
        // both.
        let option = ActionOption::new(
            "parallel-scan",
            "Parallel scan",
            vec![
                OptionStep::new("reddit-scanner", "r_MetalMusic", 40.0, vec![]),
                OptionStep::new("reddit-scanner", "r_ProgMusic", 40.0, vec![]),
                OptionStep::new("signal-inviter", "both", 20.0, vec![0, 1]),
            ],
        );

        // Both step 0 and step 1 are Ready (no dependencies).
        assert_eq!(option.steps[0].status, OptionStepStatus::Ready);
        assert_eq!(option.steps[1].status, OptionStepStatus::Ready);
        // Step 2 depends on both → Pending.
        assert_eq!(option.steps[2].status, OptionStepStatus::Pending);
    }

    #[test]
    fn parallel_steps_join_after_both_complete() {
        let mut planner = OptionPlanner::new();
        planner.start_option(ActionOption::new(
            "parallel-scan",
            "Parallel scan",
            vec![
                OptionStep::new("reddit-scanner", "r_MetalMusic", 40.0, vec![]),
                OptionStep::new("reddit-scanner", "r_ProgMusic", 40.0, vec![]),
                OptionStep::new("signal-inviter", "both", 20.0, vec![0, 1]),
            ],
        ));

        // Dispatch step 0, it succeeds. Step 2 is NOT ready yet (step 1 still
        // pending).
        {
            let option = planner
                .active_options
                .iter_mut()
                .find(|o| o.id == "parallel-scan")
                .unwrap();
            option.steps[0].status = OptionStepStatus::Dispatched;
        }
        let next = planner.advance("parallel-scan", true);
        // Step 1 is already Ready, so advance returns it.
        assert!(next.is_some());
        assert_eq!(next.unwrap().template_id, "reddit-scanner");
        let option = planner
            .active_options
            .iter()
            .find(|o| o.id == "parallel-scan")
            .unwrap();
        assert_eq!(option.steps[0].status, OptionStepStatus::Completed);
        assert_eq!(option.steps[1].status, OptionStepStatus::Ready);
        // Step 2 needs both 0 and 1 → still Pending.
        assert_eq!(option.steps[2].status, OptionStepStatus::Pending);

        // Dispatch step 1, it succeeds. Now step 2 becomes Ready.
        {
            let option = planner
                .active_options
                .iter_mut()
                .find(|o| o.id == "parallel-scan")
                .unwrap();
            option.steps[1].status = OptionStepStatus::Dispatched;
        }
        let next = planner.advance("parallel-scan", true);
        assert!(next.is_some());
        assert_eq!(next.unwrap().template_id, "signal-inviter");
        let option = planner
            .active_options
            .iter()
            .find(|o| o.id == "parallel-scan")
            .unwrap();
        assert_eq!(option.steps[2].status, OptionStepStatus::Ready);

        // Dispatch step 2, it succeeds → option completed.
        {
            let option = planner
                .active_options
                .iter_mut()
                .find(|o| o.id == "parallel-scan")
                .unwrap();
            option.steps[2].status = OptionStepStatus::Dispatched;
        }
        let next = planner.advance("parallel-scan", true);
        assert!(next.is_none());
        let option = planner
            .active_options
            .iter()
            .find(|o| o.id == "parallel-scan")
            .unwrap();
        assert_eq!(option.status, OptionStatus::Completed);
    }

    #[test]
    fn advance_returns_none_for_unknown_option() {
        let mut planner = OptionPlanner::new();
        assert!(planner.advance("does-not-exist", true).is_none());
    }

    #[test]
    fn advance_on_completed_option_returns_none() {
        let mut planner = OptionPlanner::new();
        planner.start_option(ActionOption::new(
            "single",
            "Single",
            vec![OptionStep::new(
                "reddit-scanner",
                "r_MetalMusic",
                10.0,
                vec![],
            )],
        ));
        {
            let option = planner
                .active_options
                .iter_mut()
                .find(|o| o.id == "single")
                .unwrap();
            option.steps[0].status = OptionStepStatus::Dispatched;
        }
        let _ = planner.advance("single", true);
        // Option is now Completed.
        assert!(planner.advance("single", true).is_none());
    }

    #[test]
    fn advance_with_no_dispatched_step_returns_ready_step() {
        let mut planner = OptionPlanner::new();
        planner.start_option(sequential_option());
        // Nothing is dispatched yet → advance has nothing to mark.
        // It should still not complete the option (steps are Ready, not
        // Completed) and return the first ready step.
        let next = planner.advance("reddit-metal-pipeline", true);
        // Since nothing was dispatched, no step is marked completed, but step 0
        // is Ready so it's returned.
        assert!(next.is_some());
    }

    #[test]
    fn active_and_completed_counts_track_multiple_options() {
        let mut planner = OptionPlanner::new();
        planner.start_option(sequential_option());
        planner.start_option(ActionOption::new(
            "other",
            "Other",
            vec![OptionStep::new(
                "reddit-scanner",
                "r_ProgMusic",
                10.0,
                vec![],
            )],
        ));
        assert_eq!(planner.active_count(), 2);
        assert_eq!(planner.completed_count(), 0);

        // Complete the "other" single-step option.
        {
            let option = planner
                .active_options
                .iter_mut()
                .find(|o| o.id == "other")
                .unwrap();
            option.steps[0].status = OptionStepStatus::Dispatched;
        }
        let _ = planner.advance("other", true);

        assert_eq!(planner.active_count(), 1);
        assert_eq!(planner.completed_count(), 1);
    }

    #[test]
    fn option_step_has_no_dependencies() {
        let step = OptionStep::new("reddit-scanner", "r_MetalMusic", 10.0, vec![]);
        assert!(step.has_no_dependencies());

        let step = OptionStep::new("community-engager", "r_MetalMusic", 10.0, vec![0]);
        assert!(!step.has_no_dependencies());
    }

    #[test]
    fn option_step_new_defaults_to_pending() {
        let step = OptionStep::new("reddit-scanner", "r_MetalMusic", 10.0, vec![]);
        assert_eq!(step.status, OptionStepStatus::Pending);
        assert_eq!(step.template_id, "reddit-scanner");
        assert_eq!(step.target, "r_MetalMusic");
        assert_eq!(step.expected_fans, 10.0);
    }

    #[test]
    fn option_status_default_is_pending() {
        assert_eq!(OptionStatus::default(), OptionStatus::Pending);
    }

    #[test]
    fn option_step_status_default_is_pending() {
        assert_eq!(OptionStepStatus::default(), OptionStepStatus::Pending);
    }

    #[test]
    fn action_option_serializes_to_json() {
        let option = sequential_option();
        let json = serde_json::to_string(&option).expect("should serialize");
        assert!(json.contains("reddit-metal-pipeline"));
        assert!(json.contains("reddit-scanner"));
        assert!(json.contains("\"status\":\"pending\""));
    }

    #[test]
    fn option_planner_serializes_to_json() {
        let mut planner = OptionPlanner::new();
        planner.start_option(sequential_option());
        let json = serde_json::to_string(&planner).expect("should serialize");
        assert!(json.contains("active_options"));
        assert!(json.contains("reddit-metal-pipeline"));
    }

    #[test]
    fn compute_total_expected_fans_helper() {
        let steps = vec![
            OptionStep::new("a", "t1", 100.0, vec![]),
            OptionStep::new("b", "t2", 100.0, vec![0]),
            OptionStep::new("c", "t3", 100.0, vec![1]),
        ];
        let total = compute_total_expected_fans(&steps);
        // 100 + 100*0.85 + 100*0.85 = 100 + 85 + 85 = 270
        assert!((total - 270.0).abs() < 0.001);
    }
}
