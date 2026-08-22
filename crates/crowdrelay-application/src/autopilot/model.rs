//! Stable application-boundary types for ViryaOS Autopilot.

use crowdrelay_domain::{
    AutopilotActionId, BeaconId, BookingTargetId, CityId, ContentSourceId, EventId, ExperimentId,
    ExperimentVariantId, FanId, GrowthMetricSeriesId, MerchProductId, MerchVariantId,
    OutreachOpportunityId, OutreachTargetId, PromotionCampaignId, ReleasePlanId, TeamOpportunityId,
    TicketTypeId,
    audience_lifecycle::FanLifecyclePolicy,
    autonomy::{AutonomyLevel, Confidence, PolicyDisposition},
    beacons::{BeaconCampaignPolicy, BeaconOutreachPhase},
    booking::{BookingOpportunityPolicy, BookingOutreachPhase},
    campaign_lifecycle::{EventCampaignPhase, EventCampaignPolicy},
    content_supply::{ContentArtifactKind, ContentSupplyPolicy},
    experimentation::ExperimentPolicy,
    funding::FundingPolicy,
    growth_metrics::{GrowthMetricPolicy, GrowthSignal, MetricPlatform},
    live_opportunities::{LiveOpportunityKind, LiveOpportunityPolicy},
    merch_bundle::MerchBundlePolicy,
    merchandising::{MerchPricePolicy, MerchReorderPolicy},
    outreach::{OutreachPhase, OutreachPolicy},
    pricing::TicketYieldPolicy,
    promotion::PromotionBudgetPolicy,
    release_autopilot::{ReleaseAutopilotPolicy, ReleaseMilestone},
    show_growth::{ShowGrowthLever, ShowGrowthPolicy},
    show_operations::{ShowOperationsPolicy, ShowTaskKind},
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutopilotContext {
    TicketYield,
    FanLifecycle,
    CampaignLifecycle,
    Merchandising,
    MerchPricing,
    MerchBundle,
    BookingOpportunity,
    Outreach,
    ContentSupply,
    PromotionBudget,
    Experimentation,
    ShowOperations,
    Release,
    LiveOpportunity,
    Funding,
    Beacon,
    ShowGrowth,
    GrowthMetrics,
}

impl AutopilotContext {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TicketYield => "ticket_yield",
            Self::FanLifecycle => "fan_lifecycle",
            Self::CampaignLifecycle => "campaign_lifecycle",
            Self::Merchandising => "merchandising",
            Self::MerchPricing => "merch_pricing",
            Self::MerchBundle => "merch_bundle",
            Self::BookingOpportunity => "booking_opportunity",
            Self::Outreach => "outreach",
            Self::ContentSupply => "content_supply",
            Self::PromotionBudget => "promotion_budget",
            Self::Experimentation => "experimentation",
            Self::ShowOperations => "show_operations",
            Self::Release => "release",
            Self::LiveOpportunity => "live_opportunity",
            Self::Funding => "funding",
            Self::Beacon => "beacon",
            Self::ShowGrowth => "show_growth",
            Self::GrowthMetrics => "growth_metrics",
        }
    }
}

/// Typed bounded-context configuration loaded from the policy store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutopilotPolicyConfig {
    TicketYield(TicketYieldPolicy),
    FanLifecycle(FanLifecyclePolicy),
    CampaignLifecycle(EventCampaignPolicy),
    Merchandising(MerchReorderPolicy),
    MerchPricing(MerchPricePolicy),
    MerchBundle(MerchBundlePolicy),
    BookingOpportunity(BookingOpportunityPolicy),
    Outreach(OutreachPolicy),
    ContentSupply(ContentSupplyPolicy),
    PromotionBudget(PromotionBudgetPolicy),
    Experimentation(ExperimentPolicy),
    ShowOperations(ShowOperationsPolicy),
    Release(ReleaseAutopilotPolicy),
    LiveOpportunity(LiveOpportunityPolicy),
    Funding(FundingPolicy),
    Beacon(BeaconCampaignPolicy),
    ShowGrowth(ShowGrowthPolicy),
    GrowthMetrics(GrowthMetricPolicy),
}

/// Persisted authority configuration for one bounded context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutopilotPolicy {
    pub context: AutopilotContext,
    pub enabled: bool,
    pub autonomy_level: AutonomyLevel,
    pub minimum_confidence: Confidence,
    pub max_actions_24h: u32,
    pub config: AutopilotPolicyConfig,
    /// Monotonic configuration version used to make decision evidence immutable.
    pub version: i64,
    pub guarded_until: Option<OffsetDateTime>,
    pub guardrail_reason: Option<String>,
}

/// Generic subject reference used only at the application/action boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionSubject {
    TicketType(TicketTypeId),
    Fan(FanId),
    MerchVariant(MerchVariantId),
    MerchProduct(MerchProductId),
    City(CityId),
    Event(EventId),
    OutreachOpportunity(OutreachOpportunityId),
    ContentSource(ContentSourceId),
    Experiment(ExperimentId),
    PromotionCampaign(PromotionCampaignId),
    ReleasePlan(ReleasePlanId),
    TeamOpportunity(TeamOpportunityId),
    Beacon(BeaconId),
    GrowthMetricSeries(GrowthMetricSeriesId),
}

impl ActionSubject {
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::TicketType(_) => "ticket_type",
            Self::Fan(_) => "fan",
            Self::MerchVariant(_) => "merch_variant",
            Self::MerchProduct(_) => "merch_product",
            Self::City(_) => "city",
            Self::Event(_) => "event",
            Self::OutreachOpportunity(_) => "outreach_opportunity",
            Self::ContentSource(_) => "content_source",
            Self::Experiment(_) => "experiment",
            Self::PromotionCampaign(_) => "promotion_campaign",
            Self::ReleasePlan(_) => "release_plan",
            Self::TeamOpportunity(_) => "team_opportunity",
            Self::Beacon(_) => "beacon",
            Self::GrowthMetricSeries(_) => "growth_metric_series",
        }
    }

    #[must_use]
    pub fn uuid(self) -> uuid::Uuid {
        match self {
            Self::TicketType(id) => id.into_uuid(),
            Self::Fan(id) => id.into_uuid(),
            Self::MerchVariant(id) => id.into_uuid(),
            Self::MerchProduct(id) => id.into_uuid(),
            Self::City(id) => id.into_uuid(),
            Self::Event(id) => id.into_uuid(),
            Self::OutreachOpportunity(id) => id.into_uuid(),
            Self::ContentSource(id) => id.into_uuid(),
            Self::Experiment(id) => id.into_uuid(),
            Self::PromotionCampaign(id) => id.into_uuid(),
            Self::ReleasePlan(id) => id.into_uuid(),
            Self::TeamOpportunity(id) => id.into_uuid(),
            Self::Beacon(id) => id.into_uuid(),
            Self::GrowthMetricSeries(id) => id.into_uuid(),
        }
    }
}

/// Typed executable intents. Infrastructure serializes these only at the
/// durable action boundary; decision services never manipulate JSON blobs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExperimentAllocation {
    pub variant_id: ExperimentVariantId,
    pub allocation_basis_points: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutopilotActionPayload {
    ChangeTicketPrice {
        ticket_type_id: TicketTypeId,
        from_minor: i64,
        to_minor: i64,
    },
    ChangeTicketCapacity {
        ticket_type_id: TicketTypeId,
        from_capacity: u32,
        to_capacity: u32,
        guardrail_version: i64,
    },
    RequestFanLifecycleMessage {
        fan_id: FanId,
        template_key: String,
    },
    RequestMerchReorder {
        variant_id: MerchVariantId,
        quantity: u32,
    },
    ChangeMerchPrice {
        product_id: MerchProductId,
        from_minor: i64,
        to_minor: i64,
        economics_version: i64,
    },
    RequestBookingOutreach {
        city_id: CityId,
        target_id: BookingTargetId,
        target_version: i64,
        target_name: String,
        score: u16,
        phase: BookingOutreachPhase,
    },
    RequestAudienceCampaign {
        event_id: EventId,
        phase: EventCampaignPhase,
        template_key: String,
    },
    RequestMerchBundle {
        product_a: MerchProductId,
        product_b: MerchProductId,
        bundle_price_minor: i64,
        affinity_basis_points: u16,
    },
    RequestOutreach {
        opportunity_id: OutreachOpportunityId,
        target_id: OutreachTargetId,
        target_version: i64,
        target_name: String,
        phase: OutreachPhase,
        template_key: String,
    },
    RequestBeaconDiscovery {
        event_id: EventId,
        target_count: u16,
    },
    RequestBeaconOutreach {
        beacon_id: BeaconId,
        event_id: EventId,
        beacon_version: i64,
        phase: BeaconOutreachPhase,
        template_key: String,
    },
    RequestShowGrowth {
        event_id: EventId,
        lever: ShowGrowthLever,
        template_key: String,
    },
    RequestContentArtifact {
        source_id: ContentSourceId,
        source_version: i64,
        artifact: ContentArtifactKind,
        template_key: String,
    },
    AdjustExperiment {
        experiment_id: ExperimentId,
        expected_version: i64,
        winner_variant_id: ExperimentVariantId,
        allocations: Vec<ExperimentAllocation>,
        complete: bool,
    },
    CompleteShowTask {
        event_id: EventId,
        task: ShowTaskKind,
    },
    EscalateShowTask {
        event_id: EventId,
        task: ShowTaskKind,
    },
    RequestPromotionBudgetChange {
        campaign_id: PromotionCampaignId,
        from_minor: i64,
        to_minor: i64,
        roas_basis_points: u32,
    },
    ExecuteReleaseMilestone {
        release_id: ReleasePlanId,
        title: String,
        release_at: time::OffsetDateTime,
        milestone: ReleaseMilestone,
    },
    ApplyLiveOpportunity {
        opportunity_id: TeamOpportunityId,
        opportunity_kind: LiveOpportunityKind,
        score: u16,
    },
    PrepareFundingPackage {
        opportunity_id: TeamOpportunityId,
    },
    SubmitFundingApplication {
        opportunity_id: TeamOpportunityId,
    },
    /// Surfaces a detected movement in an external metric as durable work. The
    /// payload carries the evidence and the class of response, never a
    /// provider call: what a platform can actually do is not the domain's to
    /// assume, so execution stays with the operator or an executor that owns
    /// that capability.
    RaiseGrowthOpportunity {
        series_id: GrowthMetricSeriesId,
        platform: MetricPlatform,
        metric_key: String,
        signal: GrowthSignal,
        recommended_action: String,
        /// Measured deviation from the series' own baseline, in basis points.
        deviation_basis_points: u32,
        priority: u16,
        template_key: String,
    },
    SendTeamAssignmentEmail {
        assignment_id: uuid::Uuid,
        recipient_email: String,
        recipient_name: String,
        task_title: String,
        task_detail: String,
        due_at: Option<time::OffsetDateTime>,
        action_url_path: String,
        reminder_number: u8,
    },
}

impl AutopilotActionPayload {
    #[must_use]
    pub const fn action_kind(&self) -> &'static str {
        match self {
            Self::ChangeTicketPrice { .. } => "ticket.price.change",
            Self::ChangeTicketCapacity { .. } => "ticket.capacity.change",
            Self::RequestFanLifecycleMessage { .. } => "fan.lifecycle.message.request",
            Self::RequestMerchReorder { .. } => "merch.reorder.request",
            Self::ChangeMerchPrice { .. } => "merch.price.change",
            Self::RequestBookingOutreach { .. } => "booking.outreach.request",
            Self::RequestAudienceCampaign { .. } => "audience.campaign.request",
            Self::RequestMerchBundle { .. } => "merch.bundle.request",
            Self::RequestOutreach { .. } => "outreach.request",
            Self::RequestBeaconDiscovery { .. } => "beacon.discovery.request",
            Self::RequestBeaconOutreach { .. } => "beacon.outreach.request",
            Self::RequestShowGrowth { .. } => "show.growth.request",
            Self::RequestContentArtifact { .. } => "content.artifact.request",
            Self::AdjustExperiment {
                complete: false, ..
            } => "experiment.allocation.change",
            Self::AdjustExperiment { complete: true, .. } => "experiment.complete",
            Self::CompleteShowTask { .. } => "show.task.complete",
            Self::EscalateShowTask { .. } => "show.task.escalate",
            Self::RequestPromotionBudgetChange { .. } => "promotion.budget_change.request",
            Self::ExecuteReleaseMilestone { .. } => "release.milestone.execute",
            Self::ApplyLiveOpportunity { .. } => "opportunity.live.apply",
            Self::PrepareFundingPackage { .. } => "funding.package.prepare",
            Self::SubmitFundingApplication { .. } => "funding.application.submit",
            Self::RaiseGrowthOpportunity { .. } => "growth.opportunity.raise",
            Self::SendTeamAssignmentEmail { .. } => "team.assignment.email",
        }
    }
}

/// Action-ready decision emitted by application orchestration.
#[derive(Clone, Debug, Serialize)]
pub struct DecisionCandidate {
    #[serde(skip)]
    pub context: AutopilotContext,
    #[serde(skip)]
    pub subject: ActionSubject,
    pub decision_kind: &'static str,
    pub confidence: Confidence,
    pub disposition: PolicyDisposition,
    pub reason: &'static str,
    pub input_snapshot: serde_json::Value,
    pub policy_snapshot: serde_json::Value,
    pub action: AutopilotActionPayload,
    /// Dedupe key for equivalent evidence. Changes when relevant input or policy changes.
    pub decision_key: String,
    /// Stable side-effect key. Intentionally independent from decision history.
    pub action_idempotency_key: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CandidatePersistence {
    pub decision_created: bool,
    pub action_created: bool,
    pub quota_throttled: bool,
}

/// Action claimed for execution by the worker.
#[derive(Clone, Debug)]
pub struct ClaimedAutopilotAction {
    pub id: AutopilotActionId,
    pub payload: AutopilotActionPayload,
    pub attempt_number: u32,
}
