//! Infrastructure ports used by Autopilot evaluation, execution and measurement.

use async_trait::async_trait;
use crowdrelay_domain::{
    AutopilotActionId, AutopilotMeasurementId, PlayId, WorkspaceId,
    action_class::ActionClass,
    audience_lifecycle::FanLifecycleSnapshot,
    autonomy::AutonomyLevel,
    beacons::{BeaconCampaignSnapshot, BeaconDiscoverySnapshot},
    booking::{BookingTargetSnapshot, CityOpportunitySnapshot},
    campaign_lifecycle::EventCampaignSnapshot,
    content_supply::ContentSupplySnapshot,
    experimentation::ExperimentSnapshot,
    funding::FundingOpportunitySnapshot,
    growth_debt::GrowthDebtObservation,
    growth_envelope::{EnvelopeUsage, GrowthEnvelope},
    growth_metrics::GrowthMetricSnapshot,
    live_opportunities::LiveOpportunitySnapshot,
    merch_bundle::MerchBundleSnapshot,
    merchandising::{MerchInventorySnapshot, MerchPriceSnapshot},
    outreach::OutreachSnapshot,
    performance::{EffectDirection, EffectResult, assess_effect},
    play_measurement::{
        PlayMeasurementPolicy, PlayOutcomeInput, PlayOutcomeVerdict, assess_play_outcome,
        window_velocity_milli_per_day,
    },
    plays::PlayKind,
    pricing::TicketYieldSnapshot,
    promotion::PromotionPerformanceSnapshot,
    release_autopilot::ReleasePlanSnapshot,
    show_growth::ShowGrowthSnapshot,
    show_operations::ShowTaskSnapshot,
    target_discovery::OutreachSupplySnapshot,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::model::{
    AutopilotPolicy, CandidatePersistence, ClaimedAutopilotAction, ClaimedPlayOutcome,
    DecisionCandidate, PlayAnchor, PlayOutcomeObservation, PlayRunSnapshot, PlayStart,
    PlayStepSettlement,
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
    ) -> Result<Vec<(uuid::Uuid, u32)>, RepositoryError>;

    /// Persists the decision and, for executable dispositions, creates exactly
    /// one durable action unless an equivalent action is already in flight.
    async fn persist_candidate(
        &self,
        workspace_id: WorkspaceId,
        candidate: &DecisionCandidate,
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

/// Turns one observation into the verdict that will be stored.
///
/// A free function rather than a method so the rule stays testable without a
/// database, and so the worker cannot reach a different answer than the one the
/// domain would give.
#[must_use]
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutopilotMeasurementKind {
    TicketRevenue72h,
    MerchGrossProxy7d,
    PromotionRoas7d,
    BookingReply7d,
    OutreachReply7d,
    AudienceTicketRevenue72h,
    ShowTicketRevenue7d,
    ShowGrowthSurfaceClicks7d,
    ShowGrowthAttributedTicketOrders7d,
    GrassrootsActivationReplies14d,
}

impl AutopilotMeasurementKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TicketRevenue72h => "ticket_revenue_72h",
            Self::MerchGrossProxy7d => "merch_gross_proxy_7d",
            Self::PromotionRoas7d => "promotion_roas_7d",
            Self::BookingReply7d => "booking_reply_7d",
            Self::OutreachReply7d => "outreach_reply_7d",
            Self::AudienceTicketRevenue72h => "audience_ticket_revenue_72h",
            Self::ShowTicketRevenue7d => "show_ticket_revenue_7d",
            Self::ShowGrowthSurfaceClicks7d => "show_growth_surface_clicks_7d",
            Self::ShowGrowthAttributedTicketOrders7d => "show_growth_attributed_ticket_orders_7d",
            Self::GrassrootsActivationReplies14d => "grassroots_activation_replies_14d",
        }
    }

    #[must_use]
    pub const fn direction(self) -> EffectDirection {
        EffectDirection::HigherIsBetter
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ClaimedAutopilotMeasurement {
    pub id: AutopilotMeasurementId,
    pub action_id: AutopilotActionId,
    pub kind: AutopilotMeasurementKind,
    pub subject_id: uuid::Uuid,
    pub baseline_value: f64,
    pub action_finished_at: OffsetDateTime,
    pub attempt_number: u32,
}

#[async_trait]
pub trait AutopilotMeasurementRepository: Send + Sync {
    async fn claim_due_measurements(
        &self,
        workspace_id: WorkspaceId,
        limit: u32,
        now: OffsetDateTime,
    ) -> Result<Vec<ClaimedAutopilotMeasurement>, RepositoryError>;

    async fn observe_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        now: OffsetDateTime,
    ) -> Result<f64, RepositoryError>;

    async fn complete_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement: &ClaimedAutopilotMeasurement,
        observed_value: f64,
        effect: EffectResult,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;

    async fn fail_measurement(
        &self,
        workspace_id: WorkspaceId,
        measurement_id: AutopilotMeasurementId,
        error_kind: &'static str,
        retryable: bool,
        now: OffsetDateTime,
    ) -> Result<(), RepositoryError>;
}

#[must_use]
pub fn assess_measurement_effect(
    measurement: &ClaimedAutopilotMeasurement,
    observed_value: f64,
) -> Option<EffectResult> {
    assess_effect(
        measurement.baseline_value,
        observed_value,
        measurement.kind.direction(),
        500,
    )
}
