//! Delayed effect measurement: the queue, its kinds, and how an observation
//! becomes a classified effect.
//!
//! Split out of `ports.rs` because this is one coherent surface — the shape of
//! a claimed measurement, what each kind means, and the single function that
//! turns an observed number into a finding. Keeping it together makes the one
//! distinction that matters here visible in a single screen: some kinds report
//! a level and some report an effect, and the two must never be classified the
//! same way.

use crate::RepositoryError;
use async_trait::async_trait;
use crowdrelay_domain::performance::{
    EffectDirection, EffectResult, assess_effect, assess_signed_effect,
};
use crowdrelay_domain::{AutopilotActionId, AutopilotMeasurementId, WorkspaceId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

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
    /// Fan count delta in the 14 days after an agent dispatch. Measures
    /// whether the worker's intelligence gathering actually aggregated
    /// new fans into the fanbase.
    AgentRunFanGrowth14d,
    /// Incremental fan growth: new fans in the 14-day post-action window
    /// minus the counterfactual (pre-action daily rate × 14). This is the
    /// North Star metric — it measures causal uplift, not just correlation.
    /// The baseline_value stores the pre-action daily fan arrival rate.
    IncrementalFanGrowth14d,
    /// Signal install delta in the 7 days after an agent dispatch. Measures
    /// whether the worker's output moved fans toward the Signal app (growth).
    AgentRunSignalInstalls7d,
    /// Community engagement metric delta in the 7 days after a community
    /// engagement dispatch. Measures whether the posts produced meaningful
    /// engagement (upvotes, comments) rather than just existing.
    AgentRunCommunityEngagement7d,
    /// Durable fan growth 30 days after the measurement window. Counts fans
    /// created in the 14-day post-action window that are still active 30
    /// days after creation (not suppressed, not deleted). This is the true
    /// North Star — fans that stick, not just fans that sign up.
    DurableFanGrowth30d,
    /// Scanner discovery quality: counts the number of new outreach targets
    /// discovered by a reddit-scanner dispatch in the 14-day post-action
    /// window. Measures the scanner's proximal outcome (discovery) rather
    /// than workspace-wide fan growth — the scanner doesn't acquire fans,
    /// it finds communities.
    ScannerDiscoveryQuality14d,
    /// Strategist insight quality: counts the number of campaign insights
    /// produced by a growth-strategist dispatch in the 14-day post-action
    /// window. Measures the strategist's proximal outcome (insight
    /// production) rather than workspace-wide fan growth.
    StrategistInsightQuality14d,
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
            Self::AgentRunFanGrowth14d => "agent_run_fan_growth_14d",
            Self::IncrementalFanGrowth14d => "incremental_fan_growth_14d",
            Self::AgentRunSignalInstalls7d => "agent_run_signal_installs_7d",
            Self::AgentRunCommunityEngagement7d => "agent_run_community_engagement_7d",
            Self::DurableFanGrowth30d => "durable_fan_growth_30d",
            Self::ScannerDiscoveryQuality14d => "scanner_discovery_quality_14d",
            Self::StrategistInsightQuality14d => "strategist_insight_quality_14d",
        }
    }

    #[must_use]
    pub const fn direction(self) -> EffectDirection {
        EffectDirection::HigherIsBetter
    }

    /// Whether the observed value is an effect rather than a level.
    ///
    /// A signed kind has already had its counterfactual subtracted, so a
    /// negative reading is a result — the action did worse than doing nothing
    /// — and not a malformed measurement. Generic measurement code must ask
    /// this before classifying, because [`assess_effect`] refuses negative
    /// levels and would otherwise turn every harmful outcome into a
    /// repository error and retry it away.
    #[must_use]
    pub const fn is_signed_effect(self) -> bool {
        matches!(
            self,
            Self::IncrementalFanGrowth14d | Self::DurableFanGrowth30d
        )
    }

    /// Days of counterfactual the stored `baseline_value` rate covers.
    ///
    /// The observation subtracts `baseline_value × window` and the
    /// classification divides by the same quantity, so the window lives here
    /// rather than being spelled out at both call sites where the two could
    /// drift apart.
    #[must_use]
    pub const fn counterfactual_window_days(self) -> f64 {
        match self {
            Self::IncrementalFanGrowth14d | Self::DurableFanGrowth30d => 14.0,
            _ => 0.0,
        }
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

impl ClaimedAutopilotMeasurement {
    /// The counterfactual this measurement's effect was taken against.
    ///
    /// `baseline_value` holds a daily rate for signed kinds; the observation
    /// subtracts this product and the classification divides by it. Zero for
    /// every other kind, which compare against a level instead.
    #[must_use]
    pub fn counterfactual_value(&self) -> f64 {
        self.baseline_value * self.kind.counterfactual_window_days()
    }
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
    if measurement.kind.is_signed_effect() {
        // The observation is already an effect. Classify it against zero and
        // express it against the counterfactual it was measured against.
        return assess_signed_effect(measurement.counterfactual_value(), observed_value, 500);
    }
    assess_effect(
        measurement.baseline_value,
        observed_value,
        measurement.kind.direction(),
        500,
    )
}
