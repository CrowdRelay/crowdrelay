//! Thin orchestration from typed snapshots to durable decision candidates.

use crowdrelay_domain::{
    WorkspaceId,
    audience_lifecycle::{
        FanLifecycleDecision, FanLifecycleSnapshot, LifecycleTemplate, evaluate_fan_lifecycle,
    },
    autonomy::{PolicyDisposition, disposition},
    beacons::{
        BeaconCampaignSnapshot, BeaconDecision, BeaconDiscoveryDecision, BeaconDiscoverySnapshot,
        BeaconOutreachPhase, evaluate_beacon_campaign, evaluate_beacon_discovery,
    },
    booking::{
        BookingFollowUpDecision, BookingFollowUpPolicy, BookingOpportunityDecision,
        BookingOutreachPhase, BookingTargetDecision, BookingTargetSelectionPolicy,
        BookingTargetSnapshot, CityOpportunitySnapshot, estimated_attendance,
        evaluate_booking_followup, evaluate_booking_opportunity, select_booking_target,
    },
    campaign_lifecycle::{EventCampaignDecision, EventCampaignSnapshot, evaluate_event_campaign},
    content_supply::{ContentSupplyDecision, ContentSupplySnapshot, evaluate_content_supply},
    experimentation::{ExperimentDecision, ExperimentSnapshot, evaluate_experiment},
    funding::{FundingDecision, FundingOpportunitySnapshot, evaluate_funding},
    live_opportunities::{
        LiveOpportunityDecision, LiveOpportunitySnapshot, evaluate_live_opportunity,
    },
    merch_bundle::{MerchBundleDecision, MerchBundleSnapshot, evaluate_merch_bundle},
    merchandising::{
        MerchInventorySnapshot, MerchPriceDecision, MerchPriceDirection, MerchPriceSnapshot,
        MerchReorderDecision, evaluate_merch_price, evaluate_reorder,
    },
    outreach::{OutreachDecision, OutreachSnapshot, evaluate_outreach},
    pricing::{
        TicketAllocationDecision, TicketYieldDecision, TicketYieldSnapshot,
        evaluate_ticket_allocation, evaluate_ticket_yield,
    },
    promotion::{PromotionBudgetDecision, PromotionPerformanceSnapshot, evaluate_promotion_budget},
    release_autopilot::{ReleaseDecision, ReleaseMilestone, ReleasePlanSnapshot, evaluate_release},
    show_operations::{ShowOperationsDecision, ShowTaskSnapshot, evaluate_show_task},
};
use serde::Serialize;
use thiserror::Error;
use time::OffsetDateTime;

use super::{model::*, ports::AutopilotDecisionRepository};
mod beacons;
mod commercial;
mod growth_debt;
mod growth_metrics;
mod show_growth;

use beacons::{beacon_candidate, beacon_discovery_candidate};
use commercial::{
    booking_candidate, booking_followup_candidate, campaign_lifecycle_candidate, funding_candidate,
    merch_candidate, merch_price_candidate,
};
use growth_debt::growth_debt_candidate;
use growth_metrics::growth_metric_candidate;
use show_growth::show_growth_candidate;

use crate::RepositoryError;

pub struct EvaluateAutopilot<'a, R> {
    repository: &'a R,
    workspace_id: WorkspaceId,
}

impl<'a, R> EvaluateAutopilot<'a, R>
where
    R: AutopilotDecisionRepository,
{
    #[must_use]
    pub const fn new(repository: &'a R, workspace_id: WorkspaceId) -> Self {
        Self {
            repository,
            workspace_id,
        }
    }

    pub async fn execute(
        &self,
        now: OffsetDateTime,
    ) -> Result<AutopilotCycleReport, AutopilotError> {
        let policies = self.repository.load_policies(self.workspace_id).await?;
        let mut report = AutopilotCycleReport::default();

        for policy in policies.into_iter().filter(|policy| policy.enabled) {
            match policy.context {
                AutopilotContext::TicketYield => {
                    let snapshots = self
                        .repository
                        .load_ticket_yield_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = ticket_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut report).await?;
                        }
                        if let Some(candidate) =
                            ticket_allocation_candidate(snapshot, &policy, now)?
                        {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::FanLifecycle => {
                    let snapshots = self
                        .repository
                        .load_fan_lifecycle_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = lifecycle_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::CampaignLifecycle => {
                    let snapshots = self
                        .repository
                        .load_event_campaign_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) =
                            campaign_lifecycle_candidate(snapshot, &policy, now)?
                        {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::Merchandising => {
                    let snapshots = self
                        .repository
                        .load_merch_inventory_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = merch_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::MerchPricing => {
                    let snapshots = self
                        .repository
                        .load_merch_price_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = merch_price_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::MerchBundle => {
                    let snapshots = self
                        .repository
                        .load_merch_bundle_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = merch_bundle_candidate(snapshot, &policy)? {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::BookingOpportunity => {
                    let snapshots = self
                        .repository
                        .load_city_opportunity_snapshots(self.workspace_id, now)
                        .await?;
                    let targets = self
                        .repository
                        .load_booking_target_snapshots(self.workspace_id, now)
                        .await?;
                    for target in &targets {
                        if let Some(candidate) = booking_followup_candidate(target, &policy, now)? {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                    for snapshot in snapshots {
                        if let Some(candidate) =
                            booking_candidate(snapshot, &targets, &policy, now)?
                        {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::Outreach => {
                    let snapshots = self
                        .repository
                        .load_outreach_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = outreach_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::ContentSupply => {
                    let snapshots = self
                        .repository
                        .load_content_supply_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = content_candidate(&snapshot, &policy, now)? {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::Experimentation => {
                    let snapshots = self
                        .repository
                        .load_experiment_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = experiment_candidate(&snapshot, &policy)? {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::ShowOperations => {
                    let snapshots = self
                        .repository
                        .load_show_task_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = show_operations_candidate(snapshot, &policy, now)?
                        {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::PromotionBudget => {
                    let snapshots = self
                        .repository
                        .load_promotion_performance_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = promotion_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::Release => {
                    let snapshots = self
                        .repository
                        .load_release_plan_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = release_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::LiveOpportunity => {
                    let snapshots = self
                        .repository
                        .load_live_opportunity_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = live_opportunity_candidate(snapshot, &policy, now)?
                        {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::Funding => {
                    let snapshots = self
                        .repository
                        .load_funding_opportunity_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = funding_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::Beacon => {
                    let discovery = self
                        .repository
                        .load_beacon_discovery_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in discovery {
                        if let Some(candidate) = beacon_discovery_candidate(snapshot, &policy, now)?
                        {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                    let snapshots = self
                        .repository
                        .load_beacon_campaign_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = beacon_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::ShowGrowth => {
                    let snapshots = self
                        .repository
                        .load_show_growth_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in snapshots {
                        if let Some(candidate) = show_growth_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::GrowthMetrics => {
                    let snapshots = self
                        .repository
                        .load_growth_metric_snapshots(self.workspace_id, now)
                        .await?;
                    for snapshot in &snapshots {
                        if let Some(candidate) = growth_metric_candidate(snapshot, &policy, now)? {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                }
                AutopilotContext::GrowthDebt => {
                    let observations = self
                        .repository
                        .load_growth_debt_observations(self.workspace_id, now)
                        .await?;
                    for observation in &observations {
                        if let Some(candidate) = growth_debt_candidate(observation, &policy, now)? {
                            self.persist(&candidate, &mut report).await?;
                        }
                    }
                }
            }
        }

        Ok(report)
    }

    async fn persist(
        &self,
        candidate: &DecisionCandidate,
        report: &mut AutopilotCycleReport,
    ) -> Result<(), AutopilotError> {
        let persisted = self
            .repository
            .persist_candidate(self.workspace_id, candidate)
            .await?;
        if persisted.decision_created {
            report.decisions = report.decisions.saturating_add(1);
        }
        if persisted.action_created {
            report.actions_enqueued = report.actions_enqueued.saturating_add(1);
        }
        if persisted.quota_throttled {
            report.actions_throttled = report.actions_throttled.saturating_add(1);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AutopilotCycleReport {
    pub decisions: u32,
    pub actions_enqueued: u32,
    pub actions_throttled: u32,
}

#[derive(Debug, Error)]
pub enum AutopilotError {
    #[error("autopilot repository failed")]
    Repository(#[from] RepositoryError),
    #[error("autopilot decision serialization failed")]
    Serialization(#[from] serde_json::Error),
}

include!("evaluate/candidates.rs");
include!("evaluate/tests.rs");
include!("evaluate/growth_metrics_tests.rs");
include!("evaluate/growth_debt_tests.rs");
