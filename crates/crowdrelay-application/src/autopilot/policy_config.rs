//! Operator-tunable configuration per autopilot context.
//!
//! Split from `model.rs` when the file reached its source-size ratchet
//! ceiling — the enum and its `parse_for` read path are one unit, and the
//! ratchet exists to keep `model.rs` from growing into everything.

use super::AutopilotContext;
use crowdrelay_brain::GrowthIntelligencePolicy;
use crowdrelay_domain::{
    audience_lifecycle::FanLifecyclePolicy,
    beacons::BeaconCampaignPolicy,
    booking::BookingOpportunityPolicy,
    booking_agent::BookingAgentPolicy,
    campaign_lifecycle::EventCampaignPolicy,
    content_engine::ContentStrategyPolicy,
    content_supply::ContentSupplyPolicy,
    experimentation::ExperimentPolicy,
    funding::FundingPolicy,
    growth_debt::GrowthDebtPolicy,
    growth_metrics::GrowthMetricPolicy,
    live_opportunities::LiveOpportunityPolicy,
    merch_bundle::MerchBundlePolicy,
    merchandising::{MerchPricePolicy, MerchReorderPolicy},
    outreach::OutreachPolicy,
    plays::PlayPolicy,
    pricing::TicketYieldPolicy,
    promotion::PromotionBudgetPolicy,
    release_autopilot::ReleaseAutopilotPolicy,
    representation::RepresentationPolicy,
    roster_weekly_brief::RosterPolicy,
    show_growth::ShowGrowthPolicy,
    show_operations::ShowOperationsPolicy,
    target_discovery::OutreachSupplyPolicy,
};

/// Typed bounded-context configuration loaded from the policy store.
#[derive(Clone, Debug, PartialEq)]
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
    GrowthDebt(GrowthDebtPolicy),
    OutreachSupply(OutreachSupplyPolicy),
    GrowthIntelligence(GrowthIntelligencePolicy),
    Plays(PlayPolicy),
    ContentStrategy(ContentStrategyPolicy),
    Representation(RepresentationPolicy),
    BookingAgent(BookingAgentPolicy),
    Roster(RosterPolicy),
}

impl AutopilotPolicyConfig {
    /// Parses one context's operator config from its stored JSON.
    ///
    /// The single source of truth for "what config keys does this context
    /// accept": the policy reader, the write path and the API validator all
    /// call this, so a key cannot be accepted on write and silently dropped
    /// on read. An empty object means "reset to defaults" — every knob is
    /// optional and every type carries its own defaults.
    pub fn parse_for(
        context: AutopilotContext,
        raw: serde_json::Value,
    ) -> Result<Self, serde_json::Error> {
        match context {
            AutopilotContext::TicketYield => {
                Self::parse_into(raw, Self::TicketYield, TicketYieldPolicy::default())
            }
            AutopilotContext::FanLifecycle => {
                Self::parse_into(raw, Self::FanLifecycle, FanLifecyclePolicy::default())
            }
            AutopilotContext::CampaignLifecycle => {
                Self::parse_into(raw, Self::CampaignLifecycle, EventCampaignPolicy::default())
            }
            AutopilotContext::Merchandising => {
                Self::parse_into(raw, Self::Merchandising, MerchReorderPolicy::default())
            }
            AutopilotContext::MerchPricing => {
                Self::parse_into(raw, Self::MerchPricing, MerchPricePolicy::default())
            }
            AutopilotContext::MerchBundle => {
                Self::parse_into(raw, Self::MerchBundle, MerchBundlePolicy::default())
            }
            AutopilotContext::BookingOpportunity => Self::parse_into(
                raw,
                Self::BookingOpportunity,
                BookingOpportunityPolicy::default(),
            ),
            AutopilotContext::Outreach => {
                Self::parse_into(raw, Self::Outreach, OutreachPolicy::default())
            }
            AutopilotContext::ContentSupply => {
                Self::parse_into(raw, Self::ContentSupply, ContentSupplyPolicy::default())
            }
            AutopilotContext::PromotionBudget => {
                Self::parse_into(raw, Self::PromotionBudget, PromotionBudgetPolicy::default())
            }
            AutopilotContext::Experimentation => {
                Self::parse_into(raw, Self::Experimentation, ExperimentPolicy::default())
            }
            AutopilotContext::ShowOperations => {
                let parsed =
                    Self::parse_into(raw, Self::ShowOperations, ShowOperationsPolicy::default())?;
                if let Self::ShowOperations(policy) = &parsed {
                    policy.validate().map_err(serde::de::Error::custom)?;
                }
                Ok(parsed)
            }
            AutopilotContext::Release => {
                Self::parse_into(raw, Self::Release, ReleaseAutopilotPolicy::default())
            }
            AutopilotContext::LiveOpportunity => {
                Self::parse_into(raw, Self::LiveOpportunity, LiveOpportunityPolicy::default())
            }
            AutopilotContext::Funding => {
                Self::parse_into(raw, Self::Funding, FundingPolicy::default())
            }
            AutopilotContext::Beacon => {
                Self::parse_into(raw, Self::Beacon, BeaconCampaignPolicy::default())
            }
            AutopilotContext::ShowGrowth => {
                Self::parse_into(raw, Self::ShowGrowth, ShowGrowthPolicy::default())
            }
            AutopilotContext::GrowthMetrics => {
                Self::parse_into(raw, Self::GrowthMetrics, GrowthMetricPolicy::default())
            }
            AutopilotContext::GrowthDebt => {
                Self::parse_into(raw, Self::GrowthDebt, GrowthDebtPolicy::default())
            }
            AutopilotContext::OutreachSupply => {
                Self::parse_into(raw, Self::OutreachSupply, OutreachSupplyPolicy::default())
            }
            AutopilotContext::GrowthIntelligence => Self::parse_into(
                raw,
                Self::GrowthIntelligence,
                GrowthIntelligencePolicy::default(),
            ),
            AutopilotContext::Plays => Self::parse_into(raw, Self::Plays, PlayPolicy::default()),
            AutopilotContext::ContentStrategy => {
                Self::parse_into(raw, Self::ContentStrategy, ContentStrategyPolicy::default())
            }
            AutopilotContext::Representation => {
                Self::parse_into(raw, Self::Representation, RepresentationPolicy::default())
            }
            AutopilotContext::BookingAgent => {
                Self::parse_into(raw, Self::BookingAgent, BookingAgentPolicy::default())
            }
            AutopilotContext::Roster => {
                Self::parse_into(raw, Self::Roster, RosterPolicy::default())
            }
        }
    }

    fn parse_into<T>(
        raw: serde_json::Value,
        wrap: fn(T) -> Self,
        default: T,
    ) -> Result<Self, serde_json::Error>
    where
        T: serde::de::DeserializeOwned,
    {
        // An empty object is the reset-to-defaults spelling, matching how a
        // provisioned workspace reads before anybody has tuned anything.
        if raw.as_object().is_some_and(serde_json::Map::is_empty) {
            return Ok(wrap(default));
        }
        serde_json::from_value::<T>(raw).map(wrap)
    }
}
