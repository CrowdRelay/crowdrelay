//! Split PostgreSQL Autopilot adapter implementation.

use super::*;

include!("decisions/core_reads.rs");
include!("decisions/opportunity_reads.rs");
include!("decisions/persist.rs");

// Keep the heavy SQL implementations outside the `async_trait` procedural
// attribute. Attribute macros run before nested `macro_rules!` invocations are
// expanded, so generating trait methods directly from the macros leaves those
// methods untransformed and causes E0195 lifetime mismatches. The macros now
// expand into inherent `*_impl` helpers; this explicit trait impl stays tiny
// and is fully visible to `async_trait`.
impl PostgresAutopilotRepository {
    decision_core_reads!();
    decision_opportunity_reads!();
    decision_persist!();
}

#[async_trait]
impl AutopilotDecisionRepository for PostgresAutopilotRepository {
    async fn load_policies(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<AutopilotPolicy>, RepositoryError> {
        self.load_policies_impl(workspace_id).await
    }

    async fn load_ticket_yield_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<TicketYieldSnapshot>, RepositoryError> {
        self.load_ticket_yield_snapshots_impl(workspace_id, now)
            .await
    }

    async fn load_fan_lifecycle_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<FanLifecycleSnapshot>, RepositoryError> {
        self.load_fan_lifecycle_snapshots_impl(workspace_id, now)
            .await
    }

    async fn load_event_campaign_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<EventCampaignSnapshot>, RepositoryError> {
        self.load_event_campaign_snapshots_impl(workspace_id, now)
            .await
    }

    async fn load_merch_inventory_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<MerchInventorySnapshot>, RepositoryError> {
        self.load_merch_inventory_snapshots_impl(workspace_id, now)
            .await
    }

    async fn load_merch_price_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<MerchPriceSnapshot>, RepositoryError> {
        self.load_merch_price_snapshots_impl(workspace_id, now)
            .await
    }

    async fn load_merch_bundle_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<MerchBundleSnapshot>, RepositoryError> {
        self.load_merch_bundle_snapshots_impl(workspace_id, now)
            .await
    }

    async fn load_city_opportunity_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<CityOpportunitySnapshot>, RepositoryError> {
        self.load_city_opportunity_snapshots_impl(workspace_id, now)
            .await
    }

    async fn load_booking_target_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<BookingTargetSnapshot>, RepositoryError> {
        self.load_booking_target_snapshots_impl(workspace_id, now)
            .await
    }

    async fn load_outreach_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<OutreachSnapshot>, RepositoryError> {
        self.load_outreach_snapshots_impl(workspace_id, now).await
    }

    async fn load_content_supply_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ContentSupplySnapshot>, RepositoryError> {
        self.load_content_supply_snapshots_impl(workspace_id, now)
            .await
    }

    async fn load_experiment_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ExperimentSnapshot>, RepositoryError> {
        self.load_experiment_snapshots_impl(workspace_id, now).await
    }

    async fn load_show_task_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ShowTaskSnapshot>, RepositoryError> {
        self.load_show_task_snapshots_impl(workspace_id, now).await
    }

    async fn load_promotion_performance_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<PromotionPerformanceSnapshot>, RepositoryError> {
        self.load_promotion_performance_snapshots_impl(workspace_id, now)
            .await
    }

    async fn load_release_plan_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ReleasePlanSnapshot>, RepositoryError> {
        self.load_release_plan_snapshots_impl(workspace_id, now)
            .await
    }

    async fn load_live_opportunity_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<LiveOpportunitySnapshot>, RepositoryError> {
        self.load_live_opportunity_snapshots_impl(workspace_id, now)
            .await
    }

    async fn load_deliverability_snapshot(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<DeliverabilitySnapshot, RepositoryError> {
        // The operator's own ceiling, so the ramp is measured against the
        // number they set rather than against a constant in here.
        let (envelope, _) = self.load_growth_envelope_impl(workspace_id, now).await?;
        self.load_deliverability_snapshot_impl(
            workspace_id,
            now,
            envelope.weekly_third_party_touches,
        )
        .await
    }

    async fn load_outreach_waves(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<OutreachWaveSnapshot>, RepositoryError> {
        self.load_outreach_waves_impl(workspace_id, now).await
    }

    async fn load_outreach_wave_anchors(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<OutreachWaveAnchor>, RepositoryError> {
        self.load_outreach_wave_anchors_impl(workspace_id, now)
            .await
    }

    async fn open_outreach_wave(
        &self,
        workspace_id: WorkspaceId,
        start: &OutreachWaveStart,
    ) -> Result<bool, RepositoryError> {
        self.open_outreach_wave_impl(workspace_id, start).await
    }

    async fn transition_outreach_wave(
        &self,
        workspace_id: WorkspaceId,
        wave_id: uuid::Uuid,
        transition: OutreachWaveTransition,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.transition_outreach_wave_impl(workspace_id, wave_id, transition, now)
            .await
    }

    async fn load_playlist_placements(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<PlaylistPlacementSnapshot>, RepositoryError> {
        self.load_playlist_placements_impl(workspace_id, now).await
    }

    async fn settle_playlist_placement(
        &self,
        workspace_id: WorkspaceId,
        settlement: PlacementSettlement,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.settle_playlist_placement_impl(workspace_id, settlement, now)
            .await
    }

    async fn load_live_opportunity_terms(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<LiveTermsSnapshot>, RepositoryError> {
        self.load_live_opportunity_terms_impl(workspace_id, now)
            .await
    }

    async fn settle_live_opportunity_terms(
        &self,
        workspace_id: WorkspaceId,
        settlement: &TermsSettlement,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.settle_live_opportunity_terms_impl(workspace_id, settlement, now)
            .await
    }

    async fn load_funding_opportunity_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<FundingOpportunitySnapshot>, RepositoryError> {
        self.load_funding_opportunity_snapshots_impl(workspace_id, now)
            .await
    }

    async fn load_beacon_discovery_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<BeaconDiscoverySnapshot>, RepositoryError> {
        self.load_beacon_discovery_snapshots_impl(workspace_id, now)
            .await
    }

    async fn load_beacon_campaign_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<BeaconCampaignSnapshot>, RepositoryError> {
        self.load_beacon_campaign_snapshots_impl(workspace_id, now)
            .await
    }

    async fn load_booking_supply_snapshot(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<BookingSupplySnapshot, RepositoryError> {
        self.load_booking_supply_snapshot_impl(workspace_id, now)
            .await
    }

    async fn load_beacon_invite_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<BeaconInviteSnapshot>, RepositoryError> {
        self.load_beacon_invite_snapshots_impl(workspace_id, now)
            .await
    }

    async fn load_show_growth_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<ShowGrowthSnapshot>, RepositoryError> {
        self.load_show_growth_snapshots_impl(workspace_id, now)
            .await
    }

    async fn load_growth_metric_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<GrowthMetricSnapshot>, RepositoryError> {
        self.load_growth_metric_snapshots_impl(workspace_id, now)
            .await
    }

    async fn load_growth_debt_observations(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<GrowthDebtObservation>, RepositoryError> {
        self.load_growth_debt_observations_impl(workspace_id, now)
            .await
    }

    async fn load_growth_intelligence_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<GrowthIntelligenceSnapshot>, RepositoryError> {
        self.load_growth_intelligence_snapshots_impl(workspace_id, now)
            .await
    }

    async fn mark_insights_consumed(
        &self,
        workspace_id: WorkspaceId,
        outcome_ids: &[uuid::Uuid],
    ) -> Result<u64, RepositoryError> {
        super::operations::mark_insights_consumed(self, workspace_id, outcome_ids).await
    }

    async fn load_causal_model(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<crowdrelay_brain::CausalModel, RepositoryError> {
        super::operations::load_causal_model(self, workspace_id).await
    }

    async fn record_dispatch_prediction(
        &self,
        workspace_id: WorkspaceId,
        action_id: uuid::Uuid,
        prediction: &crowdrelay_brain::DispatchPrediction,
        strategy: Option<&str>,
        holdout_probability: f64,
    ) -> Result<(), RepositoryError> {
        super::operations::record_dispatch_prediction(
            self,
            workspace_id,
            action_id,
            prediction,
            strategy,
            holdout_probability,
        )
        .await
    }

    async fn record_experiment_assignment(
        &self,
        workspace_id: WorkspaceId,
        assignment: &crowdrelay_brain::ExperimentAssignment,
        strategy: Option<&str>,
    ) -> Result<(), RepositoryError> {
        super::operations::experiment_assignments::record_experiment_assignment(
            self,
            workspace_id,
            assignment,
            strategy,
        )
        .await
    }

    async fn record_credit_allocation(
        &self,
        workspace_id: WorkspaceId,
        outcome: &crowdrelay_brain::FanOutcome,
        result: &crowdrelay_brain::AttributionResult,
        measurement_id: Option<uuid::Uuid>,
        attribution_version: u32,
    ) -> Result<(), RepositoryError> {
        super::operations::evidence::record_credit_allocation(
            self,
            workspace_id,
            outcome,
            result,
            measurement_id,
            attribution_version,
        )
        .await
    }

    async fn discover_competing_actions(
        &self,
        workspace_id: WorkspaceId,
        outcome_action_id: uuid::Uuid,
        window_start: time::OffsetDateTime,
        window_end: time::OffsetDateTime,
    ) -> Result<Vec<crowdrelay_brain::ActionExposure>, RepositoryError> {
        super::operations::evidence::discover_competing_actions(
            self,
            workspace_id,
            outcome_action_id,
            window_start,
            window_end,
        )
        .await
    }

    async fn process_attribution_batch(
        &self,
        workspace_id: WorkspaceId,
        batch_size: u32,
    ) -> Result<u32, RepositoryError> {
        super::operations::attribution::process_attribution_batch(self, workspace_id, batch_size)
            .await
    }

    async fn load_exploration_memory(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<crowdrelay_brain::ExplorationMemory, RepositoryError> {
        super::operations::load_exploration_memory(self, workspace_id).await
    }

    async fn load_last_dispatched_template(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<String>, RepositoryError> {
        super::operations::load_last_dispatched_template(self, workspace_id).await
    }

    async fn load_reach_metrics(
        &self,
        workspace_id: WorkspaceId,
        since: OffsetDateTime,
        until: Option<OffsetDateTime>,
    ) -> Result<crowdrelay_brain::ReachMetrics, RepositoryError> {
        super::operations::reach::load_reach_metrics(self, workspace_id, since, until).await
    }

    async fn record_growth_evidence(
        &self,
        workspace_id: WorkspaceId,
        evidence: &crowdrelay_brain::GrowthEvidence,
    ) -> Result<(), RepositoryError> {
        super::operations::evidence::record_growth_evidence(self, workspace_id, evidence).await
    }

    async fn load_growth_evidence(
        &self,
        workspace_id: WorkspaceId,
        since: Option<OffsetDateTime>,
    ) -> Result<Vec<crowdrelay_brain::GrowthEvidence>, RepositoryError> {
        super::operations::evidence::load_growth_evidence(self, workspace_id, since).await
    }

    async fn count_pending_measurements(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<u32, RepositoryError> {
        super::operations::evidence::count_pending_measurements(self, workspace_id).await
    }

    async fn save_brain_state(
        &self,
        workspace_id: WorkspaceId,
        module: &str,
        state: &serde_json::Value,
    ) -> Result<(), RepositoryError> {
        super::operations::evidence::save_brain_state(self, workspace_id, module, state).await
    }

    async fn load_brain_state(
        &self,
        workspace_id: WorkspaceId,
        module: &str,
    ) -> Result<Option<(serde_json::Value, OffsetDateTime)>, RepositoryError> {
        super::operations::evidence::load_brain_state(self, workspace_id, module).await
    }

    async fn save_brain_state_checkpoint(
        &self,
        workspace_id: WorkspaceId,
        model: &crowdrelay_brain::CausalModel,
    ) -> Result<(), RepositoryError> {
        super::operations::growth_intelligence::save_causal_model_checkpoint(
            self,
            workspace_id,
            model,
        )
        .await
    }

    async fn load_outreach_supply_snapshot(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<OutreachSupplySnapshot, RepositoryError> {
        self.load_outreach_supply_snapshot_impl(workspace_id, now)
            .await
    }

    async fn load_play_anchors(
        &self,
        workspace_id: WorkspaceId,
        kind: PlayKind,
        now: OffsetDateTime,
    ) -> Result<Vec<PlayAnchor>, RepositoryError> {
        self.load_play_anchors_impl(workspace_id, kind, now).await
    }

    async fn load_play_standings(
        &self,
        workspace_id: WorkspaceId,
        policy: PlayPolicy,
    ) -> Result<Vec<PlayKindStanding>, RepositoryError> {
        self.load_play_standings_impl(workspace_id, policy).await
    }

    async fn load_outreach_kind_standings(
        &self,
        workspace_id: WorkspaceId,
        max_pitches_per_wave: u32,
    ) -> Result<Vec<OutreachKindStanding>, RepositoryError> {
        self.load_outreach_kind_standings_impl(workspace_id, max_pitches_per_wave)
            .await
    }

    async fn start_play(
        &self,
        workspace_id: WorkspaceId,
        start: &PlayStart,
    ) -> Result<bool, RepositoryError> {
        self.start_play_impl(workspace_id, start).await
    }

    async fn load_play_snapshots(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<PlayRunSnapshot>, RepositoryError> {
        self.load_play_snapshots_impl(workspace_id, now).await
    }

    async fn settle_play_step(
        &self,
        workspace_id: WorkspaceId,
        settlement: &PlayStepSettlement,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.settle_play_step_impl(workspace_id, settlement, now)
            .await
    }

    async fn complete_play(
        &self,
        workspace_id: WorkspaceId,
        play_id: PlayId,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError> {
        self.complete_play_impl(workspace_id, play_id, now).await
    }

    async fn load_autonomy_ceilings(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<(ActionClass, AutonomyLevel)>, RepositoryError> {
        self.load_autonomy_ceilings_impl(workspace_id).await
    }

    async fn load_growth_envelope(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<(GrowthEnvelope, EnvelopeUsage), RepositoryError> {
        self.load_growth_envelope_impl(workspace_id, now).await
    }

    async fn load_outward_touch_ages(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<std::collections::HashMap<Uuid, u32>, RepositoryError> {
        self.load_outward_touch_ages_impl(workspace_id, now).await
    }

    async fn persist_candidate(
        &self,
        workspace_id: WorkspaceId,
        candidate: &DecisionCandidate,
    ) -> Result<CandidatePersistence, RepositoryError> {
        self.persist_candidate_impl(workspace_id, candidate).await
    }
}
