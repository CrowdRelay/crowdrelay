//! Infrastructure ports used by Autopilot evaluation, execution and measurement.

use async_trait::async_trait;
use crowdrelay_brain::{
    AttributionResult, CausalModel, DispatchPrediction, ExecutionStatus, ExperimentAssignment,
    ExperimentDesign, ExperimentUnitKind, ExplorationMemory, FanOutcome, FanProvenanceEvent,
    GrowthIntelligenceSnapshot,
};
use crowdrelay_domain::{
    AutopilotActionId, PlayId, ReleasePlanId, TraceContext, WorkspaceId,
    action_class::ActionClass,
    audience_lifecycle::FanLifecycleSnapshot,
    autonomy::AutonomyLevel,
    beacons::{BeaconCampaignSnapshot, BeaconDiscoverySnapshot, BeaconInviteSnapshot},
    booking::{BookingTargetSnapshot, CityOpportunitySnapshot},
    booking_window::BookingWindowInputSet,
    campaign_lifecycle::EventCampaignSnapshot,
    content_supply::{CommunityRelayTarget, ContentSupplySnapshot, SignalPushAudience},
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
    release_autopilot::{ReleaseMilestone, ReleasePlanSnapshot, ShowWeekCollision},
    show_growth::ShowGrowthSnapshot,
    show_operations::ShowTaskSnapshot,
    target_discovery::OutreachSupplySnapshot,
};
use time::OffsetDateTime;

use crowdrelay_domain::deliverability::DeliverabilitySnapshot;

use super::evidence_ledger::EvidenceLedger;
use super::model::{
    AutopilotContext, AutopilotPolicy, CandidatePersistence, ClaimedAutopilotAction,
    ClaimedPlayOutcome, DecisionCandidate, LiveTermsSnapshot, OutreachKindStanding,
    OutreachWaveAnchor, OutreachWaveSnapshot, OutreachWaveStart, OutreachWaveTransition,
    PlacementSettlement, PlayAnchor, PlayKindStanding, PlayOutcomeObservation, PlayRunSnapshot,
    PlayStart, PlayStepSettlement, PlaylistPlacementSnapshot, TermsSettlement,
};
use crate::RepositoryError;

/// A causal model and the identity of the belief state that produced it.
///
/// A decision persists the number the model gave it. Without this it could not
/// say which beliefs produced that number, and the posteriors move every cycle
/// — so re-deriving the estimate later answers "what would the brain predict
/// now", which is a different question that looks identical in a report.
///
/// Two fields rather than a tuple, and the identity is the repository's to
/// report rather than the caller's to derive: the checkpoint row is the
/// repository's to read, and an application layer computing an identity for
/// state it did not load would be guessing at provenance.
#[derive(Clone, Debug)]
pub struct LoadedCausalModel {
    pub model: CausalModel,
    pub belief: BeliefStateOrigin,
}

/// Where the belief state came from, and what identifies it.
///
/// Deliberately two variants. A full replay has no checkpoint, and reporting
/// one would be a fabricated identity — the honest answer is that the model was
/// rebuilt from evidence, and how much of it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum BeliefStateOrigin {
    /// Rebuilt from a stored checkpoint, then advanced by the evidence
    /// resolved since it was written.
    ///
    /// `checkpoint_content_hash` is the identity. `brain_state` holds
    /// one row per module, updated in place, with no id and no history — so
    /// `checkpoint_updated_at` says *when* and cannot say *which*, and the
    /// state it labelled is overwritten by the next cycle. The hash is derived
    /// from the content that produced the estimate, so it stays true after the
    /// row moves on: two decisions carrying the same hash used the same
    /// beliefs, and one carrying a different hash did not.
    ///
    /// It identifies, and does not retrieve. Nothing here stores the
    /// checkpoint's bytes, and claiming otherwise would be the lie this type
    /// exists to prevent.
    Checkpoint {
        checkpoint_content_hash: String,
        #[serde(with = "time::serde::rfc3339")]
        checkpoint_updated_at: OffsetDateTime,
        /// Evidence rows applied on top of the checkpoint. The estimate came
        /// from checkpoint plus these, not from the checkpoint alone.
        delta_evidence: u32,
    },
    /// No usable checkpoint existed, so the model was rebuilt from evidence.
    FullReplay { evidence_replayed: u32 },
}

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

    /// The window-proposal inputs for the same cycle's target set (§12-6):
    /// the workspace's own show calendar once, plus each venue-linked
    /// target's room history and coordinates. Targets without a venue link
    /// simply have no entry — `booking_candidate` treats that as `None`.
    async fn load_booking_window_inputs(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<BookingWindowInputSet, RepositoryError>;

    async fn load_outreach_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<OutreachSnapshot>, RepositoryError>;

    /// One snapshot per conversation still waiting on an answer — the newest
    /// unanswered inbound reply per outreach target. The outreach evaluator
    /// holds `AlreadyReplied` on these by design; this is the lane that
    /// answers them instead.
    async fn load_unanswered_reply_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<crowdrelay_domain::reply_rescue::UnansweredReplySnapshot>, RepositoryError>;

    async fn load_content_supply_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ContentSupplySnapshot>, RepositoryError>;

    /// The communities a synced band post may be relayed into — only targets
    /// the screening pipeline admitted and promotion carried. An empty list
    /// means the relay still reaches the owned audience, just nobody else's.
    async fn load_relay_community_targets(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<CommunityRelayTarget>, RepositoryError>;

    /// How many fans a Signal push to `segment` would reach right now — the
    /// same eligibility the send path applies (active fan, newest marketing
    /// consent granted, the segment's predicates, at least one live push
    /// endpoint), clamped by the workspace's per-step envelope bound. A
    /// `None` segment is the broadcast-to-consented case; a segment that
    /// cannot resolve surfaces the same refusal the send would.
    async fn load_signal_push_audience(
        &self,
        workspace_id: WorkspaceId,
        segment: Option<&str>,
    ) -> Result<SignalPushAudience, RepositoryError>;

    /// Relay pushes raised since `since` (not cancelled), for the pacing in
    /// `content_supply::relay_push_verdict`.
    async fn load_recent_relay_pushes(
        &self,
        workspace_id: WorkspaceId,
        since: OffsetDateTime,
    ) -> Result<Vec<crowdrelay_domain::content_supply::RecentRelayPush>, RepositoryError>;

    /// Failed show-growth attempts per `(event id, lever)`, so a failed lever
    /// is retried under a new key and passed over after
    /// `show_growth::MAX_LEVER_ATTEMPTS`.
    async fn load_show_growth_failures(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<std::collections::HashMap<(uuid::Uuid, String), u32>, RepositoryError>;

    /// Whether a live executor advertises `capability` — `true` when no
    /// executor has ever registered, the fail-open rule every executor gate
    /// shares.
    async fn capability_serviceable(
        &self,
        workspace_id: WorkspaceId,
        capability: &str,
    ) -> Result<bool, RepositoryError>;

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

    /// Shows whose `starts_at` sits inside `now`'s ISO week — the moments a
    /// release milestone collides with when it lands this week (§4i-2).
    /// Empty means no collision and costs one small query per cycle.
    async fn load_colliding_show_week(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ShowWeekCollision>, RepositoryError>;

    /// The recorded milestone marks for a set of release plans: what the
    /// ladder actually got to, and when. The timeline view is built from
    /// these rather than from the snapshot's done/not-done booleans, so a
    /// completion carries its real date.
    async fn load_release_milestone_marks(
        &self,
        workspace_id: WorkspaceId,
        release_ids: &[ReleasePlanId],
    ) -> Result<Vec<(ReleasePlanId, ReleaseMilestone, OffsetDateTime)>, RepositoryError>;

    /// The milestones a §4i-2 collision hold recorded for each plan — read
    /// from the decision ledger (`hold_release_milestone_collision`), because
    /// a held step never earns a milestone mark and would otherwise look
    /// merely due. The timeline renders these as held, not missing.
    async fn load_held_release_milestones(
        &self,
        workspace_id: WorkspaceId,
        release_ids: &[ReleasePlanId],
    ) -> Result<Vec<(ReleasePlanId, ReleaseMilestone)>, RepositoryError>;

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

    /// Measured standing per dispatch key (`action_kind:identity`, e.g.
    /// `agent.run.request:social-post` or `show.growth.request:partner_cross_promo`)
    /// across every measured action kind. Keys absent from the map have no
    /// measured outcomes — callers treat absence as untested, never as harm.
    async fn load_action_standings(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<
        std::collections::HashMap<String, crowdrelay_domain::learning::Standing>,
        RepositoryError,
    >;

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

    /// Returns the content suggestions still waiting on the band — rows the
    /// engine raised that have neither been answered nor expired. The
    /// context surfaces each as an approval-queue action; `approved` rows
    /// are already committed work and never re-queue.
    async fn load_open_content_suggestions(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<crowdrelay_domain::content_engine::ContentSuggestion>, RepositoryError>;

    /// Returns arcs still waiting on the band's one approval — `proposed`
    /// rows only. An approved or active arc is already the season's shape,
    /// and a retired one is a no the cooldown remembers.
    async fn load_proposed_content_arcs(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<crowdrelay_domain::content_engine::Arc>, RepositoryError>;

    /// One cycle's worth of join-ask inputs (§5): the tenant's configured
    /// variants, cadence and platforms, the connected fanbase channels, the
    /// join-ask post ledger, and whether Instagram has a photo to publish.
    ///
    /// Always a snapshot. This returned `Option` once, and `None` — "the
    /// tenant never wrote variants" — made the evaluator skip the context, so
    /// a workspace nobody had set up produced a cycle indistinguishable from
    /// a healthy one. Not emitting an *ask* for a tenant with no words was
    /// right; not reporting that it has none was not. An unconfigured tenant
    /// now resolves to `JoinAskConfig::unconfigured` and is held on
    /// `NoVariants` — the empty variant list is the first gate, so no ask can
    /// escape it.
    async fn load_join_ask_snapshot(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<crowdrelay_domain::join_ask::JoinAskSnapshot, RepositoryError>;

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
    ///
    /// Returns the identity of the belief state alongside it. See
    /// [`LoadedCausalModel`] for why the identity is the repository's to
    /// report rather than the caller's to derive.
    async fn load_causal_model(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<LoadedCausalModel, RepositoryError>;

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

    /// Counts, per context, the dispatches whose outcome was actually
    /// measured.
    ///
    /// Read once per cycle and handed to the authority gate, which until now
    /// could ask a context how confident it was but not what that confidence
    /// was computed from. Only rows carrying a resolved outcome count: a
    /// dispatch still inside its measurement window is not yet evidence about
    /// anything, and counting it would let a context earn unattended execution
    /// by acting rather than by learning.
    ///
    /// A context with no measured outcome is absent from the ledger rather
    /// than present with a zero.
    async fn load_resolved_evidence_counts(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<EvidenceLedger, RepositoryError>;

    /// Records a credit allocation — attributed credit for a fan outcome.
    /// CRITICAL: the raw observation in the evidence table is immutable.
    /// This stores attributed credit in a SEPARATE table
    /// (`fan_credit_ledger`). The learner consumes credited
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

    /// Persists a hypothesis lifecycle state transition for a template.
    /// Called after walk-forward validation degrades or promotes a
    /// template. The persisted state is loaded on the next cycle by
    /// the snapshot loader.
    async fn save_hypothesis_state(
        &self,
        workspace_id: WorkspaceId,
        template_id: &str,
        state: crowdrelay_brain::hypothesis::HypothesisState,
    ) -> Result<(), RepositoryError>;

    /// Appends belief revisions to the operator's learning record.
    ///
    /// Called after the belief itself has been saved, and never before: the
    /// ledger describes a change that already happened, so a failed write here
    /// costs the explanation and not the learning. Nothing in the brain reads
    /// these rows back.
    async fn record_belief_revisions(
        &self,
        workspace_id: WorkspaceId,
        revisions: &[super::BeliefRevision],
    ) -> Result<(), RepositoryError>;

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

    /// Loads the reply probability model from its brain-state checkpoint,
    /// then updates it from outreach interaction outcomes observed since
    /// the checkpoint. Returns the updated model.
    ///
    /// The model is an additive, reversible advisory signal: it reorders
    /// eligible outreach targets by predicted P(positive reply), it does
    /// not change eligibility, authority, or approval. An empty model
    /// (cold start) returns the global prior for every prediction, so the
    /// system falls back to `relevance_basis_points` ranking.
    async fn load_reply_model(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<crowdrelay_brain::ReplyProbabilityModel, RepositoryError>;

    /// Saves the reply probability model checkpoint for fast startup on
    /// the next cycle. Best-effort: a failed checkpoint just means the
    /// next cycle rebuilds from full history.
    async fn save_reply_model(
        &self,
        workspace_id: WorkspaceId,
        model: &crowdrelay_brain::ReplyProbabilityModel,
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

    /// Unattended actions each context has taken in the trailing seven days,
    /// for the warm-up allowance that keeps the evidence floor from sealing
    /// itself shut.
    ///
    /// Counted from the durable action rows rather than a second ledger, for
    /// the same reason the envelope counts its touches there: two ledgers can
    /// disagree, and the one that is wrong is the one nobody is reading.
    async fn load_bootstrap_spend(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<std::collections::BTreeMap<AutopilotContext, i64>, RepositoryError>;

    /// Hours since the agent last reached each subject through an outward
    /// action, for the cooldown. One query per cycle, not one per candidate.
    async fn load_outward_touch_ages(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<std::collections::HashMap<uuid::Uuid, u32>, RepositoryError>;

    /// Which of the given decision keys already name a persisted decision.
    ///
    /// The evaluator uses it to keep an already-dispatched candidate out of
    /// the portfolio pool: a candidate whose dedup key is taken can only
    /// no-op at persist, and under a health-scaled budget that no-op still
    /// occupies the slot a dispatchable candidate needed.
    async fn existing_decision_keys(
        &self,
        workspace_id: WorkspaceId,
        decision_keys: &[String],
    ) -> Result<std::collections::HashSet<String>, RepositoryError>;

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

    /// Replaces the workspace's current candidate pool with the pool the eval
    /// just ranked (5.1). The roster's pooled read re-ranks the union of every
    /// member act's rows — losers are rows too, because a candidate a lone act
    /// rejected for `max_dispatches` may win a slot in the roster's larger
    /// pool, which is the whole point of pooling.
    ///
    /// The pool is current state, not a log: each cycle's write replaces the
    /// workspace's rows atomically, so the table always holds the pool the
    /// latest cycle actually ranked.
    async fn replace_portfolio_pool(
        &self,
        workspace_id: WorkspaceId,
        entries: &[PortfolioPoolEntry],
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;
}

/// One row of a workspace's current candidate pool — the persisted form the
/// roster read (5.1) re-ranks under the organisation's own portfolio config.
///
/// `decision_value` travels whole rather than as `intrinsic_y30`: a re-rank
/// needs the same `total()`, `resource_cost`, `uncertainty` and bridge terms
/// the act's own selection used, not a number re-derived against a world
/// model that has since moved.
#[derive(Clone, Debug)]
pub struct PortfolioPoolEntry {
    /// `OpportunityId`'s canonical string — the key rejections already use,
    /// so a row's local outcome joins to the same identity.
    pub opportunity_key: String,
    /// The identity itself, whole: its display form is not parseable (parts
    /// contain `:`), so the roster read rebuilds candidates from this.
    pub opportunity_id: crowdrelay_brain::OpportunityId,
    pub audience_key: String,
    pub source_context: String,
    pub action_key: String,
    pub decision_value: crowdrelay_brain::DecisionValue,
    pub is_experimental: bool,
    /// What the act's own selection did with this candidate — kept so the
    /// roster read can say "locally rejected for `max_dispatches`, selected
    /// here" rather than presenting a number without its history.
    pub selected: bool,
    pub rejection_reason: Option<String>,
}

/// The `last_error_kind` an action is failed with when the roster's monthly
/// attention budget refused the send (§4d-3).
///
/// Recorded on the action row, which bounds the column at 96 characters — so
/// this is a short kind, not a sentence. It lives here rather than beside the
/// query that raises it because the worker records it and the repository
/// raises it, and neither should own the other's vocabulary.
///
/// Raised as `RepositoryError::ConflictBecause` by `reserve_contact_window`
/// so it survives the trip through `execute_action` to `fail_action`. A plain
/// `Conflict` would read back as `state_changed`, and "the organization
/// already spent this person's monthly share" is not a stale-write retry —
/// the lapsed/attention reads must be able to tell it apart.
pub const ORG_ATTENTION_BUDGET_ERROR_KIND: &str = "org_attention_budget";

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

    /// Record only the supplied claim's failure. A reclaimed or completed
    /// attempt must remain untouched by a late worker.
    async fn fail_action(
        &self,
        workspace_id: WorkspaceId,
        action_id: AutopilotActionId,
        attempt_number: u32,
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

/// What the reply's target is, at the granularity the triage loop needs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplyTargetKind {
    /// One of the outreach kinds — the classifier's vocabulary applies.
    Outreach(OutreachTargetKind),
    /// A promoter, venue, or festival on the booking channel. A negotiation
    /// reply is
    /// always a human's call: the operator filed the disposition with the
    /// reply, and the number inside the text is a proposal to confirm, not
    /// a disposition to infer.
    BookingCounterparty,
}

/// A reply awaiting first-party classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplyNeedingTriage {
    pub reply_id: uuid::Uuid,
    pub target_id: uuid::Uuid,
    pub target_kind: ReplyTargetKind,
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

mod booking_discovery;
pub use booking_discovery::*;
