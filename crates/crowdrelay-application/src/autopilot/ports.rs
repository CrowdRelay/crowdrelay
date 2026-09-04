//! Infrastructure ports used by Autopilot evaluation, execution and measurement.

use async_trait::async_trait;
use crowdrelay_brain::{
    AttributionResult, CausalModel, DispatchPrediction, ExecutionStatus, ExperimentAssignment,
    ExperimentDesign, ExperimentUnitKind, ExplorationMemory, FanOutcome, FanProvenanceEvent,
    GrowthIntelligenceSnapshot,
};
use crowdrelay_domain::{
    AutopilotActionId, PlayId, TraceContext, WorkspaceId,
    action_class::ActionClass,
    audience_lifecycle::FanLifecycleSnapshot,
    autonomy::AutonomyLevel,
    beacons::{BeaconCampaignSnapshot, BeaconDiscoverySnapshot, BeaconInviteSnapshot},
    booking::{BookingTargetSnapshot, CityOpportunitySnapshot},
    campaign_lifecycle::EventCampaignSnapshot,
    content_supply::ContentSupplySnapshot,
    experimentation::ExperimentSnapshot,
    funding::FundingOpportunitySnapshot,
    growth_debt::GrowthDebtObservation,
    growth_envelope::{EnvelopeUsage, GrowthEnvelope},
    growth_metrics::GrowthMetricSnapshot,
    learning::{WaveOutcomeVerdict, WaveReplyCounts, assess_wave_outcome},
    live_opportunities::LiveOpportunitySnapshot,
    merch_bundle::MerchBundleSnapshot,
    merchandising::{MerchInventorySnapshot, MerchPriceSnapshot},
    outreach::{OutreachSnapshot, OutreachTargetKind},
    play_measurement::{
        PlayMeasurementPolicy, PlayOutcomeInput, PlayOutcomeVerdict, assess_play_outcome,
        window_velocity_milli_per_day,
    },
    plays::{PlayKind, PlayPolicy},
    pricing::TicketYieldSnapshot,
    promotion::PromotionPerformanceSnapshot,
    release_autopilot::ReleasePlanSnapshot,
    show_growth::ShowGrowthSnapshot,
    show_operations::ShowTaskSnapshot,
    target_discovery::OutreachSupplySnapshot,
};
use time::OffsetDateTime;

use crowdrelay_domain::deliverability::DeliverabilitySnapshot;

use super::model::{
    AutopilotPolicy, CandidatePersistence, ClaimedAutopilotAction, ClaimedPlayOutcome,
    DecisionCandidate, LiveTermsSnapshot, OutreachKindStanding, OutreachWaveAnchor,
    OutreachWaveSnapshot, OutreachWaveStart, OutreachWaveTransition, PlacementSettlement,
    PlayAnchor, PlayKindStanding, PlayOutcomeObservation, PlayRunSnapshot, PlayStart,
    PlayStepSettlement, PlaylistPlacementSnapshot, TermsSettlement,
};
use crate::RepositoryError;

#[async_trait]
pub trait AutopilotDecisionRepository: Send + Sync {
    async fn load_policies(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<AutopilotPolicy>, RepositoryError>;

    async fn load_ticket_yield_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<TicketYieldSnapshot>, RepositoryError>;

    async fn load_fan_lifecycle_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<FanLifecycleSnapshot>, RepositoryError>;

    async fn load_event_campaign_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<EventCampaignSnapshot>, RepositoryError>;

    async fn load_merch_inventory_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<MerchInventorySnapshot>, RepositoryError>;

    async fn load_merch_price_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<MerchPriceSnapshot>, RepositoryError>;

    async fn load_merch_bundle_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<MerchBundleSnapshot>, RepositoryError>;

    async fn load_city_opportunity_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<CityOpportunitySnapshot>, RepositoryError>;

    async fn load_booking_target_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<BookingTargetSnapshot>, RepositoryError>;

    async fn load_outreach_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<OutreachSnapshot>, RepositoryError>;

    async fn load_content_supply_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ContentSupplySnapshot>, RepositoryError>;

    async fn load_experiment_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ExperimentSnapshot>, RepositoryError>;

    async fn load_show_task_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ShowTaskSnapshot>, RepositoryError>;

    async fn load_promotion_performance_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<PromotionPerformanceSnapshot>, RepositoryError>;

    async fn load_release_plan_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ReleasePlanSnapshot>, RepositoryError>;

    async fn load_live_opportunity_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<LiveOpportunitySnapshot>, RepositoryError>;

    /// What this workspace's sending looks like from outside: how much it has
    /// sent, how much of that bounced or was reported, and when it started.
    async fn load_deliverability_snapshot(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<DeliverabilitySnapshot, RepositoryError>;

    /// Free-reach waves still being drafted or waiting on a human.
    async fn load_outreach_waves(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<OutreachWaveSnapshot>, RepositoryError>;

    /// Anchors — releases and shows — with no wave of a given kind yet.
    async fn load_outreach_wave_anchors(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<OutreachWaveAnchor>, RepositoryError>;

    /// Opens a wave. False when another cycle got there first, which is not a
    /// failure: the unique constraint is what makes one wave per anchor true.
    async fn open_outreach_wave(
        &self,
        workspace_id: WorkspaceId,
        start: &OutreachWaveStart,
    ) -> Result<bool, RepositoryError>;

    /// Seals a wave for review, or ends it without approval.
    async fn transition_outreach_wave(
        &self,
        workspace_id: WorkspaceId,
        wave_id: uuid::Uuid,
        transition: OutreachWaveTransition,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    /// Claimed placements that are not settled yet.
    async fn load_playlist_placements(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<PlaylistPlacementSnapshot>, RepositoryError>;

    /// Ends a placement. A withdrawal also suppresses the curator behind it and
    /// every other target sharing their identity, in the same transaction:
    /// suppressing the playlist and leaving the operator pitchable is how the
    /// same person is approached again next week through a different list.
    async fn settle_playlist_placement(
        &self,
        workspace_id: WorkspaceId,
        settlement: PlacementSettlement,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    /// Live negotiations, each with the show it is about.
    async fn load_live_opportunity_terms(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<LiveTermsSnapshot>, RepositoryError>;

    /// Ends a negotiation without an acceptance. Idempotent on the state
    /// already being unsettled, so two cycles racing on the same expired window
    /// leave one recorded reason.
    async fn settle_live_opportunity_terms(
        &self,
        workspace_id: WorkspaceId,
        settlement: &TermsSettlement,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    async fn load_funding_opportunity_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<FundingOpportunitySnapshot>, RepositoryError>;

    async fn load_beacon_discovery_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<BeaconDiscoverySnapshot>, RepositoryError>;

    async fn load_beacon_campaign_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<BeaconCampaignSnapshot>, RepositoryError>;

    /// How many bookable targets the pipeline can contact, and when the agent
    /// last asked for more supply. The booking analogue of outreach supply.
    async fn load_booking_supply_snapshot(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<crowdrelay_domain::booking_discovery::BookingSupplySnapshot, RepositoryError>;

    /// Verified scene nodes with an upcoming show in their own city, and how
    /// long since — or whether — they were last asked to run invite codes.
    async fn load_beacon_invite_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<BeaconInviteSnapshot>, RepositoryError>;

    async fn load_show_growth_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ShowGrowthSnapshot>, RepositoryError>;

    /// Returns every active metric series with its derived trend and the two
    /// pieces of context the rule needs but cannot see from one series alone:
    /// how long ago this series last produced a decision, and whether the same
    /// platform has a stronger-tier series being tracked.
    async fn load_growth_metric_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<GrowthMetricSnapshot>, RepositoryError>;

    /// Returns one observation per subject that has outstanding committed work,
    /// across every debt kind, already carrying `hours_since_last_signal`.
    ///
    /// The adapter reports facts only — how long the work has been outstanding,
    /// how much of it is outstanding, and what date applies. Every horizon and
    /// threshold lives in `GrowthDebtPolicy`, so what counts as neglect can
    /// change without touching a query.
    async fn load_growth_debt_observations(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<GrowthDebtObservation>, RepositoryError>;

    /// Returns one snapshot per worker template that the brain may dispatch.
    /// Each snapshot carries the hours since the last run and the workspace's
    /// current situation (upcoming events, fan growth, unengaged targets).
    /// The deterministic evaluator uses these to decide whether to dispatch.
    async fn load_growth_intelligence_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<GrowthIntelligenceSnapshot>, RepositoryError>;

    /// Marks agent outcomes as consumed by the brain. Called after the
    /// evaluator has factored the insights into its dispatch decisions.
    /// Consumed rows are deleted by the retention worker after 7 days.
    async fn mark_insights_consumed(
        &self,
        workspace_id: WorkspaceId,
        outcome_ids: &[uuid::Uuid],
    ) -> Result<u64, RepositoryError>;

    /// Loads the causal model from past dispatch predictions and their
    /// measured outcomes. The brain uses this to predict how many fans
    /// each worker dispatch will produce, and to learn from prediction
    /// errors (the dopamine loop).
    async fn load_causal_model(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<CausalModel, RepositoryError>;

    /// Records a first-class experiment assignment. The experimental unit
    /// is explicitly defined (audience, community, campaign, etc.) — not
    /// always workspace-wide. When `is_interference_controllable` is false,
    /// the assignment is recorded as a matched quasi-experiment.
    async fn record_experiment_assignment(
        &self,
        workspace_id: WorkspaceId,
        assignment: &ExperimentAssignment,
        strategy: Option<&str>,
    ) -> Result<(), RepositoryError>;

    /// Transitions the execution_status of an experiment assignment.
    ///
    /// Monotonic: only `dispatched → executed` and `dispatched → failed`
    /// are allowed. All other transitions are silently no-ops (the DB
    /// WHERE clause prevents them). This is the one transition point
    /// from the executor/result path.
    ///
    /// Retry-safe: setting the same status is a no-op (idempotent).
    async fn update_execution_status(
        &self,
        workspace_id: WorkspaceId,
        assignment_id: &str,
        new_status: ExecutionStatus,
    ) -> Result<(), RepositoryError>;

    /// Transitions the execution_status of an experiment assignment by
    /// looking up the assignment via `action_id`.
    ///
    /// This is the primary transition path for the community executor:
    /// when `community_posts.status` becomes `'posted'`, the executor
    /// calls this with `ExecutionStatus::Executed`. When the post
    /// definitively fails, it calls with `ExecutionStatus::Failed`. When
    /// confirmation is lost (stale posting from a worker crash), it calls
    /// with `ExecutionStatus::Unknown`.
    ///
    /// Monotonic: only `dispatched → executed`, `dispatched → failed`,
    /// and `dispatched → unknown` are allowed. `unknown` is non-terminal
    /// and can later resolve to `executed` or `failed` via reconciliation.
    /// All other transitions are silently no-ops.
    async fn update_execution_status_by_action_id(
        &self,
        workspace_id: WorkspaceId,
        action_id: uuid::Uuid,
        new_status: ExecutionStatus,
    ) -> Result<(), RepositoryError>;

    /// Get-or-creates a persisted experiment design.
    ///
    /// P0-1: The experiment identity must survive evaluator retries. The
    /// same `(workspace, intervention_key, logical_cycle_key)` always
    /// converges on the same `experiment_uuid`. On first call, a new design
    /// is inserted. On retry/concurrent call, the existing design is
    /// returned. The DB unique index on
    /// `(workspace_id, intervention_key, logical_cycle_key)` is the
    /// convergence guarantee.
    ///
    /// The returned design carries the stable `experiment_uuid` that all
    /// assignments for this logical cycle must use.
    #[allow(clippy::too_many_arguments)]
    async fn get_or_create_experiment_design(
        &self,
        workspace_id: WorkspaceId,
        intervention_key: &str,
        logical_cycle_key: &str,
        unit_kind: ExperimentUnitKind,
        eligible_units: Vec<String>,
        holdout_probability: f64,
        strategy: &str,
        min_eligible_units: u32,
        min_expected_control: u32,
        min_expected_treatment: u32,
        now: time::OffsetDateTime,
    ) -> Result<ExperimentDesign, RepositoryError>;

    /// Atomically persists a treatment action AND its experiment assignment
    /// in a single database transaction.
    ///
    /// P0-2: The system must never reach a state where an action exists but
    /// its experiment assignment is missing. This method commits the
    /// decision + action + idempotency + outbox + experiment assignment as
    /// one atomic state transition. If any step fails, the entire
    /// transaction rolls back.
    ///
    /// The `assignment` is constructed with `action_id: None` by the caller;
    /// this method fills in the real `action_id` from the inserted action
    /// before recording the assignment.
    #[allow(clippy::too_many_arguments)]
    async fn persist_treatment_with_assignment(
        &self,
        workspace_id: WorkspaceId,
        candidate: &DecisionCandidate,
        assignment: &ExperimentAssignment,
        prediction: &DispatchPrediction,
        strategy: Option<&str>,
        holdout_probability: f64,
        trace: &TraceContext,
    ) -> Result<CandidatePersistence, RepositoryError>;

    /// Records a fan provenance event — an append-only exposure/
    /// interaction/conversion/durability event. PROVENANCE ≠ CAUSALITY:
    /// these events establish exposure/attribution evidence, not causal
    /// treatment effect.
    async fn record_fan_provenance_event(
        &self,
        workspace_id: WorkspaceId,
        event: &FanProvenanceEvent,
    ) -> Result<(), RepositoryError>;

    /// Loads the strongest evidence quality for a template+unit from
    /// the experiment assignment state. Returns `Observational` when
    /// no experiment assignments exist.
    async fn load_evidence_quality(
        &self,
        workspace_id: WorkspaceId,
        template_id: &str,
        unit_id: &str,
    ) -> Result<crowdrelay_brain::EvidenceQuality, RepositoryError>;

    /// Loads the contamination estimate for a unit+template from the
    /// experiment assignment state. Returns 0.0 when no assignments exist.
    async fn load_contamination_estimate(
        &self,
        workspace_id: WorkspaceId,
        template_id: &str,
        unit_id: &str,
    ) -> Result<f64, RepositoryError>;

    /// Loads the calibration bias for a template from the calibration
    /// tracker in brain state. Returns 0.0 when no calibration data exists.
    async fn load_calibration_bias(
        &self,
        workspace_id: WorkspaceId,
        template_id: &str,
    ) -> Result<f64, RepositoryError>;

    /// Records a credit allocation — attributed credit for a fan outcome.
    /// CRITICAL: the raw observation in the evidence table is immutable.
    /// This stores attributed credit in a SEPARATE table
    /// (`viryaos_fan_credit_ledger`). The learner consumes credited
    /// effects from the credit ledger, not raw observations.
    async fn record_credit_allocation(
        &self,
        workspace_id: WorkspaceId,
        outcome: &FanOutcome,
        result: &AttributionResult,
        measurement_id: Option<uuid::Uuid>,
        attribution_version: u32,
    ) -> Result<(), RepositoryError>;

    /// Discovers competing actions for attribution — all treatment
    /// evidence rows in the same workspace whose dispatch window overlaps
    /// with the outcome's measurement window. Used by the attribution
    /// worker to construct `ActionExposure` vectors for the
    /// `CreditAllocator`.
    async fn discover_competing_actions(
        &self,
        workspace_id: WorkspaceId,
        outcome_action_id: uuid::Uuid,
        window_start: OffsetDateTime,
        window_end: OffsetDateTime,
    ) -> Result<Vec<crowdrelay_brain::ActionExposure>, RepositoryError>;

    /// Processes a batch of pending attribution requests. Claims pending
    /// requests from the outbox, discovers competing actions, runs the
    /// `ProportionalCreditAllocator`, and writes credited entries to the
    /// credit ledger. Returns the number of requests processed.
    async fn process_attribution_batch(
        &self,
        workspace_id: WorkspaceId,
        batch_size: u32,
    ) -> Result<u32, RepositoryError>;

    /// Loads the exploration memory from past dispatch predictions.
    /// The brain uses this to compute novelty: unexplored (template,
    /// context) pairs get an exploration bonus in the EFE score.
    async fn load_exploration_memory(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<ExplorationMemory, RepositoryError>;

    /// Loads the most recently dispatched template's ID, used to infer
    /// the previous growth strategy for hysteresis. Returns `None` if
    /// no dispatches have been recorded yet.
    async fn load_last_dispatched_template(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<String>, RepositoryError>;

    /// Loads aggregated reach metrics for the unified reach ledger.
    /// Returns counts of each status type (sent, delivered, opened,
    /// clicked, replied, converted, etc.) and the total estimated reach
    /// within the time window.
    async fn load_reach_metrics(
        &self,
        workspace_id: WorkspaceId,
        since: OffsetDateTime,
        until: Option<OffsetDateTime>,
    ) -> Result<crowdrelay_brain::ReachMetrics, RepositoryError>;

    /// Loads resolved growth evidence for the brain's learning loop.
    /// Returns only evidence rows that have a resolved outcome
    /// (observed_fans, observed_incremental_fans, or durable_fans_30d).
    /// Ordered oldest-first so the brain can replay in chronological order.
    async fn load_growth_evidence(
        &self,
        workspace_id: WorkspaceId,
        since: Option<OffsetDateTime>,
    ) -> Result<Vec<crowdrelay_brain::GrowthEvidence>, RepositoryError>;

    /// Counts unresolved growth evidence rows — dispatches whose outcomes
    /// haven't been observed yet (resolved_at IS NULL). Used by the WAIT
    /// candidate's value-of-information computation: pending measurements
    /// have epistemic value because the brain can learn from their outcomes
    /// before committing to new dispatches.
    async fn count_pending_measurements(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<u32, RepositoryError>;

    /// Saves a brain state checkpoint (serialized CausalModel) for fast
    /// startup. The brain loads the checkpoint on restart and applies
    /// only delta evidence (evidence with timestamp > checkpoint).
    async fn save_brain_state(
        &self,
        workspace_id: WorkspaceId,
        module: &str,
        state: &serde_json::Value,
    ) -> Result<(), RepositoryError>;

    /// Loads a brain state checkpoint. Returns the serialized state and
    /// its timestamp, or None if no checkpoint exists.
    async fn load_brain_state(
        &self,
        workspace_id: WorkspaceId,
        module: &str,
    ) -> Result<Option<(serde_json::Value, OffsetDateTime)>, RepositoryError>;

    /// Saves a causal model checkpoint for fast startup with delta replay.
    /// Called after each autopilot cycle. Best-effort: a failed checkpoint
    /// just means the next cycle does a full replay.
    async fn save_brain_state_checkpoint(
        &self,
        workspace_id: WorkspaceId,
        model: &crowdrelay_brain::CausalModel,
    ) -> Result<(), RepositoryError>;

    /// What the pitcher currently has to work with. One row per workspace
    /// rather than a list: supply is not a property of any single target, and
    /// counting it per target is how a starved pipeline stays invisible.
    async fn load_outreach_supply_snapshot(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<OutreachSupplySnapshot, RepositoryError>;

    /// Anchors that could carry a play of this kind and do not have one yet.
    ///
    /// Scoped by kind because "already has a play" is per kind: a show may run
    /// a track-us play and a listing sweep, and one query that ignored the kind
    /// would start the second only until the first existed.
    async fn load_play_anchors(
        &self,
        workspace_id: WorkspaceId,
        kind: PlayKind,
        now: OffsetDateTime,
    ) -> Result<Vec<PlayAnchor>, RepositoryError>;

    /// What the measured record says about each kind of play.
    ///
    /// Read once per cycle rather than per anchor: a standing is a property of
    /// the play kind, and re-reading it for every show would be a query per
    /// candidate for an answer that cannot change mid-cycle.
    async fn load_play_standings(
        &self,
        workspace_id: WorkspaceId,
        policy: PlayPolicy,
    ) -> Result<Vec<PlayKindStanding>, RepositoryError>;

    /// The outreach kind standings, with the operator's wave-size ceiling.
    /// Read once per cycle: a standing is a property of the target kind, and
    /// re-reading it for every wave would be a query per kind for an answer
    /// that cannot change mid-cycle.
    async fn load_outreach_kind_standings(
        &self,
        workspace_id: WorkspaceId,
        max_pitches_per_wave: u32,
    ) -> Result<Vec<OutreachKindStanding>, RepositoryError>;

    /// Creates the play and its whole step schedule in one transaction.
    ///
    /// Returns false when a play already covered this anchor. Not an error: two
    /// cycles racing, or a restart mid-cycle, must leave one play rather than a
    /// failure somebody has to interpret.
    async fn start_play(
        &self,
        workspace_id: WorkspaceId,
        start: &PlayStart,
    ) -> Result<bool, RepositoryError>;

    /// Every running play with the state its next decision needs, including who
    /// its open step has not yet reached.
    async fn load_play_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<PlayRunSnapshot>, RepositoryError>;

    /// Settles a step that will never be delivered, with its reason.
    ///
    /// The write that makes an omission a fact. Without it a step nobody
    /// approved simply stops being mentioned, which is the failure mode the
    /// whole design exists to avoid.
    async fn settle_play_step(
        &self,
        workspace_id: WorkspaceId,
        settlement: &PlayStepSettlement,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    /// Marks a play whose every step is settled as complete.
    async fn complete_play(
        &self,
        workspace_id: WorkspaceId,
        play_id: PlayId,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    /// The operator's autonomy ceiling per action class.
    ///
    /// A class missing from the returned map is read as its safest ceiling, not
    /// as an absent limit: a migration that has not run must never be a grant
    /// of authority.
    async fn load_autonomy_ceilings(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<(ActionClass, AutonomyLevel)>, RepositoryError>;

    /// The operator's volume limits, and what the workspace has already spent
    /// against them in the trailing seven days.
    ///
    /// Returned together because they are read together once per cycle: the
    /// limits without the spend cannot decide anything, and reading the spend
    /// per candidate would be a query per finding.
    async fn load_growth_envelope(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<(GrowthEnvelope, EnvelopeUsage), RepositoryError>;

    /// Hours since the agent last reached each subject through an outward
    /// action, for the cooldown. One query per cycle, not one per candidate.
    async fn load_outward_touch_ages(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<std::collections::HashMap<uuid::Uuid, u32>, RepositoryError>;

    /// Persists the decision and, for executable dispositions, creates exactly
    /// one durable action unless an equivalent action is already in flight.
    async fn persist_candidate(
        &self,
        workspace_id: WorkspaceId,
        candidate: &DecisionCandidate,
        trace: &TraceContext,
    ) -> Result<CandidatePersistence, RepositoryError>;

    /// Persists a candidate with dispatch prediction and initial growth
    /// evidence in the same transaction. Used by the non-experiment path
    /// where there is no experiment assignment but the prediction and
    /// evidence still need to be atomic with the action.
    ///
    /// P1: The prediction and initial evidence commit atomically with
    /// the action. This guarantees the prediction consistency invariant:
    /// `prediction_at_decision == prediction_persisted_in_initial_evidence`.
    async fn persist_candidate_with_evidence(
        &self,
        workspace_id: WorkspaceId,
        candidate: &DecisionCandidate,
        prediction: &DispatchPrediction,
        strategy: Option<&str>,
        holdout_probability: f64,
        trace: &TraceContext,
    ) -> Result<CandidatePersistence, RepositoryError>;
}

/// Durable execution port. Kept separate from decision snapshot access so the
/// evaluator cannot accidentally grow side-effect responsibilities.
#[async_trait]
pub trait AutopilotActionRepository: Send + Sync {
    async fn claim_due_actions(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedAutopilotAction>, RepositoryError>;

    async fn execute_action(
        &self,
        workspace_id: WorkspaceId,
        action: &ClaimedAutopilotAction,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    async fn fail_action(
        &self,
        workspace_id: WorkspaceId,
        action_id: AutopilotActionId,
        error_kind: &'static str,
        retryable: bool,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;
}

/// Settling what a play did, one claim at a time.
///
/// Kept apart from [`AutopilotMeasurementRepository`] because the two measure
/// different things and must not be confused. That one measures an *action*
/// against a metric it moved directly. This one measures a *play* — a campaign
/// of many sends — and its answers are claims with a named strength, including
/// the answer "this cannot be known, and here is why".
#[async_trait]
pub trait AutopilotPlayOutcomeRepository: Send + Sync {
    async fn claim_due_play_outcomes(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedPlayOutcome>, RepositoryError>;

    /// Reads the window. Never writes, and never fills a gap: a missing series,
    /// an ambiguous one and an absent join key all come back as themselves.
    async fn observe_play_outcome(
        &self,
        workspace_id: WorkspaceId,
        outcome: &ClaimedPlayOutcome,
        now: OffsetDateTime,
    ) -> Result<PlayOutcomeObservation, RepositoryError>;

    async fn complete_play_outcome(
        &self,
        workspace_id: WorkspaceId,
        outcome: &ClaimedPlayOutcome,
        observation: &PlayOutcomeObservation,
        verdict: PlayOutcomeVerdict,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    async fn fail_play_outcome(
        &self,
        workspace_id: WorkspaceId,
        outcome_id: uuid::Uuid,
        error_kind: &'static str,
        retryable: bool,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;
}

/// A wave outcome that is due for settlement.
#[derive(Clone, Debug)]
pub struct ClaimedWaveOutcome {
    pub id: uuid::Uuid,
    pub wave_id: uuid::Uuid,
    pub target_kind: OutreachTargetKind,
    pub pitches_sent: u32,
    pub window_start: OffsetDateTime,
    pub window_end: OffsetDateTime,
    pub attempt_number: u32,
}

/// The reply counts read from the interaction table for one wave.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaveOutcomeObservation {
    pub positive_replies: u32,
    pub declined_replies: u32,
    pub do_not_contact_replies: u32,
    pub total_replies: u32,
    pub observed_at: OffsetDateTime,
}

/// Settling what a wave did, one wave at a time.
///
/// Kept apart from [`AutopilotPlayOutcomeRepository`] because a wave measures
/// replies directly — no metric series, no baseline, no trend — and confusing
/// the two would make a wave read a play's series or vice versa.
#[async_trait]
pub trait AutopilotWaveOutcomeRepository: Send + Sync {
    /// Claims due wave outcomes for settlement. Same `FOR UPDATE SKIP LOCKED`
    /// pattern as play outcomes: two workers never settle the same wave.
    async fn claim_due_wave_outcomes(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedWaveOutcome>, RepositoryError>;

    /// Reads the reply counts for the wave's targets in the window. Never
    /// writes, and never fills a gap: a wave with no replies is `NoReplies`,
    /// not zero-against-a-baseline.
    async fn observe_wave_outcome(
        &self,
        workspace_id: WorkspaceId,
        outcome: &ClaimedWaveOutcome,
        now: OffsetDateTime,
    ) -> Result<WaveOutcomeObservation, RepositoryError>;

    /// Completes the outcome and folds the verdict into the per-kind learning
    /// record, in one transaction.
    async fn complete_wave_outcome(
        &self,
        workspace_id: WorkspaceId,
        outcome: &ClaimedWaveOutcome,
        observation: &WaveOutcomeObservation,
        verdict: WaveOutcomeVerdict,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    async fn fail_wave_outcome(
        &self,
        workspace_id: WorkspaceId,
        outcome_id: uuid::Uuid,
        error_kind: &'static str,
        retryable: bool,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;
}

/// Turns one wave observation into the verdict that will be stored.
///
/// A free function rather than a method so the rule stays testable without a
/// database, and so the worker cannot reach a different answer than the one the
/// domain would give.
#[must_use]
pub fn assess_wave_claim(
    outcome: &ClaimedWaveOutcome,
    observation: &WaveOutcomeObservation,
) -> WaveOutcomeVerdict {
    assess_wave_outcome(
        WaveReplyCounts {
            positive: observation.positive_replies,
            declined: observation.declined_replies,
            do_not_contact: observation.do_not_contact_replies,
            total: observation.total_replies,
        },
        outcome.pitches_sent,
    )
}

// ---------------------------------------------------------------------------
// Reply triage — first-party classification of inbound replies.
//
// n8n posts replies with a disposition it assigned. When the disposition is
// `Received` (unclassified), the worker re-classifies using the domain
// classifier and records the result. Replies that need human review are
// surfaced via the operator brief.
// ---------------------------------------------------------------------------

/// A reply awaiting first-party classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplyNeedingTriage {
    pub reply_id: uuid::Uuid,
    pub target_id: uuid::Uuid,
    pub target_kind: OutreachTargetKind,
    pub reply_text: String,
    pub previous_disposition: Option<crowdrelay_domain::outreach::OutreachReplyDisposition>,
}

/// The result of classifying one reply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplyTriageResult {
    pub classification: crowdrelay_domain::reply_triage::ReplyClassification,
    pub classified_at: OffsetDateTime,
}

#[async_trait]
pub trait AutopilotReplyTriageRepository: Send + Sync {
    /// Loads replies with `Received` disposition that have not been classified
    /// by the first-party classifier yet. Bounded by `limit`.
    async fn load_replies_needing_triage(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
    ) -> Result<Vec<ReplyNeedingTriage>, RepositoryError>;

    /// Records the classification for a reply and updates the reply's
    /// disposition if the classifier produced an auto-classification.
    /// For `NeedsHuman`, the disposition stays `Received` and the
    /// classification is stored for the operator brief to surface.
    async fn record_reply_classification(
        &self,
        workspace_id: WorkspaceId,
        reply_id: uuid::Uuid,
        result: &ReplyTriageResult,
    ) -> Result<(), RepositoryError>;
}
///
/// A free function rather than a method so the rule stays testable without a
/// database, and so the worker cannot reach a different answer than the one the
/// domain would give.
pub fn assess_play_claim(
    outcome: &ClaimedPlayOutcome,
    observation: &PlayOutcomeObservation,
    policy: PlayMeasurementPolicy,
) -> PlayOutcomeVerdict {
    assess_play_outcome(
        PlayOutcomeInput {
            claim: outcome.claim,
            recipients_reached: observation.recipients_reached,
            baseline_milli_per_day: outcome.baseline_milli_per_day,
            window_milli_per_day: observation.observed_value.and_then(|observed| {
                outcome.baseline_value.and_then(|baseline| {
                    window_velocity_milli_per_day(
                        baseline,
                        observed,
                        observation.observed_at - outcome.window_start,
                    )
                })
            }),
            attributed_clicks: observation.attributed_clicks,
            direction: observation.direction,
            ambiguous_series: observation.ambiguous_series,
        },
        policy,
    )
}
/// Venue/promoter discovery: the booking pipeline's supply, screened on write.
#[async_trait]
pub trait AutopilotBookingDiscoveryRepository: Send + Sync {
    async fn ingest_booking_candidates(
        &self,
        workspace_id: WorkspaceId,
        candidates: Vec<crowdrelay_domain::booking_discovery::BookingCandidateInput>,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<BookingCandidateIngestion, RepositoryError>;

    /// Promotes one admitted email-route candidate into a real booking target.
    /// The human confirmation is what turns a published route into somebody
    /// the agent may approach.
    async fn confirm_booking_candidate(
        &self,
        workspace_id: WorkspaceId,
        candidate_id: crowdrelay_domain::OutreachOpportunityId,
        idempotency_key: &crate::IdempotencyKey,
        request_id: Option<&crate::RequestId>,
    ) -> Result<crate::autopilot::AutopilotControlMutation, RepositoryError>;

    async fn list_booking_candidates(
        &self,
        workspace_id: WorkspaceId,
        status: Option<String>,
        limit: u32,
    ) -> Result<Vec<BookingCandidateView>, RepositoryError>;
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct BookingCandidateIngestion {
    pub reported: u32,
    pub admitted: u32,
    pub refused: u32,
    /// Found through a second source: contact identity dedupes, so the same
    /// inbox is never two prospects.
    pub duplicates: u32,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct BookingCandidateView {
    pub candidate_id: uuid::Uuid,
    pub target_kind: String,
    pub display_name: String,
    pub city_slug: Option<String>,
    pub route_kind: String,
    pub route_value: String,
    pub source: String,
    pub fit_basis_points: u16,
    pub status: String,
    pub refusal_reason: Option<String>,
    pub booking_target_id: Option<uuid::Uuid>,
}
