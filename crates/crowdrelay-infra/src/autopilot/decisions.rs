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
    ) -> Result<Vec<(Uuid, u32)>, RepositoryError> {
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
