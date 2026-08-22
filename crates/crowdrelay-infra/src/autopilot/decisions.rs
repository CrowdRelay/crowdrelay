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

    async fn persist_candidate(
        &self,
        workspace_id: WorkspaceId,
        candidate: &DecisionCandidate,
    ) -> Result<CandidatePersistence, RepositoryError> {
        self.persist_candidate_impl(workspace_id, candidate).await
    }
}
