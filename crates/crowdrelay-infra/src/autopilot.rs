//! PostgreSQL adapter for the deterministic ViryaOS Autopilot.

mod actions;
mod control;
mod decisions;
mod measurement;
mod operations;
mod runtime;
mod state;

use std::{collections::HashMap, future::Future, time::Duration};

use async_trait::async_trait;
use crowdrelay_application::{IdempotencyKey, RequestId};
use crowdrelay_application::{
    RepositoryError,
    autopilot::{
        AutopilotActionPayload, AutopilotActionRepository, AutopilotBookingStateRepository,
        AutopilotContext, AutopilotControlMutation, AutopilotControlOverview,
        AutopilotControlRepository, AutopilotDecisionRepository, AutopilotMarketStateRepository,
        AutopilotMeasurementKind, AutopilotMeasurementRepository, AutopilotMerchStateRepository,
        AutopilotPolicy, AutopilotPolicyConfig, AutopilotPolicySummary, AutopilotRuntimeRepository,
        AutopilotTicketStateRepository, BookingTargetMutation, CandidatePersistence,
        CityMarketSignalMutation, ClaimedAutopilotAction, ClaimedAutopilotMeasurement,
        DecisionCandidate, ExecutionReportMutation, ExecutorHeartbeatMutation,
        ExecutorReportStatus, MerchProductEconomicsMutation, PendingAutopilotAction,
        PromotionBudgetGuardrailMutation, PromotionBudgetGuardrailSummary,
        PromotionCampaignStateMutation, RecentAutopilotAction, RecentAutopilotDecision,
        RecentAutopilotEffect, RecordExecutionReport, RecordExecutorHeartbeat, RecordRumSample,
        ReleaseComponentMutation, ReleaseComponentSummary, ReleaseLedgerOverview, RumMetricSummary,
        SetAutopilotAuthority, TicketAllocationGuardrailMutation, UpsertBookingTarget,
        UpsertCityMarketSignal, UpsertMerchProductEconomics, UpsertPromotionBudgetGuardrail,
        UpsertPromotionCampaignState, UpsertReleaseComponent, UpsertTicketAllocationGuardrail,
    },
};
use crowdrelay_domain::{
    AutopilotActionId, AutopilotDecisionId, AutopilotMeasurementId, BookingTargetId, CityId,
    EventId, FanId, MarketSignalId, MerchProductId, MerchVariantId, PromotionCampaignId,
    ReleasePlanId, TeamOpportunityId, TicketTypeId, WorkspaceId,
    audience_lifecycle::{FanLifecyclePolicy, FanLifecycleSnapshot},
    autonomy::{AutonomyLevel, Confidence, PolicyDisposition},
    booking::{
        BookingOpportunityPolicy, BookingOutreachPhase, BookingReplyDisposition, BookingTargetKind,
        BookingTargetSnapshot, CityOpportunitySnapshot,
    },
    campaign_lifecycle::{EventCampaignPolicy, EventCampaignSnapshot},
    content_supply::{ContentSupplyPolicy, ContentSupplySnapshot},
    experimentation::{ExperimentPolicy, ExperimentSnapshot},
    funding::{FundingOpportunitySnapshot, FundingPolicy},
    live_opportunities::{LiveOpportunityKind, LiveOpportunityPolicy, LiveOpportunitySnapshot},
    market_intelligence::{CityMarketSignal, CityMarketSignalKind, aggregate_city_market_evidence},
    merch_bundle::{MerchBundlePolicy, MerchBundleSnapshot},
    merchandising::{
        MerchInventorySnapshot, MerchPricePolicy, MerchPriceSnapshot, MerchReorderPolicy,
    },
    outreach::{OutreachPolicy, OutreachSnapshot},
    performance::{EffectAssessment, EffectResult},
    pricing::{TicketYieldPolicy, TicketYieldSnapshot},
    promotion::{PromotionBudgetPolicy, PromotionPerformanceSnapshot},
    release_autopilot::{ReleaseAutopilotPolicy, ReleaseMilestoneHistory, ReleasePlanSnapshot},
    show_operations::{ShowOperationsPolicy, ShowTaskSnapshot},
};
use serde_json::{Value, json};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use tokio::time::timeout;
use uuid::Uuid;

use crate::{
    config::DatabaseConfig,
    database::{SqlxErrorClass, classify_sqlx_error},
};

const MAX_SNAPSHOTS_PER_CONTEXT: i64 = 500;
const EXTERNAL_ACTION_EVENT_VERSION: i32 = 1;

#[derive(Clone, Debug)]
pub struct PostgresAutopilotRepository {
    pool: PgPool,
    operation_timeout: Duration,
    lock_timeout: Duration,
}

impl PostgresAutopilotRepository {
    #[must_use]
    pub fn new(pool: PgPool, database: &DatabaseConfig) -> Self {
        Self {
            pool,
            operation_timeout: database.operation_timeout,
            lock_timeout: database.lock_timeout,
        }
    }

    #[must_use]
    pub fn new_with_timeouts(
        pool: PgPool,
        operation_timeout: Duration,
        lock_timeout: Duration,
    ) -> Self {
        Self {
            pool,
            operation_timeout,
            lock_timeout,
        }
    }

    async fn bounded<T>(
        &self,
        operation: impl Future<Output = Result<T, RepositoryError>>,
    ) -> Result<T, RepositoryError> {
        timeout(self.operation_timeout, operation)
            .await
            .map_err(|_| RepositoryError::Unavailable)?
    }
}

#[derive(Debug, FromRow)]
struct PolicyRow {
    context: String,
    enabled: bool,
    autonomy_level: String,
    minimum_confidence_basis_points: i32,
    max_actions_24h: i32,
    config: Value,
    version: i64,
    guarded_until: Option<OffsetDateTime>,
    guardrail_reason: Option<String>,
}

#[derive(Debug, FromRow)]
struct TicketSnapshotRow {
    ticket_type_id: Uuid,
    current_price_minor: i64,
    paid_quantity: i64,
    capacity: i64,
    sale_capacity: i64,
    paid_last_72h: i64,
    days_to_event: i64,
    last_price_change_at: Option<OffsetDateTime>,
    last_capacity_change_at: Option<OffsetDateTime>,
    allocation_minimum_capacity: Option<i32>,
    allocation_maximum_capacity: Option<i32>,
    allocation_step_capacity: Option<i32>,
    allocation_guardrail_version: Option<i64>,
}

#[derive(Debug, FromRow)]
struct LifecycleSnapshotRow {
    fan_id: Uuid,
    active: bool,
    marketing_consent: bool,
    created_at: OffsetDateTime,
    synesthesia_completed_at: Option<OffsetDateTime>,
    last_marketing_touch_at: Option<OffsetDateTime>,
    has_paid_ticket: bool,
    last_paid_ticket_at: Option<OffsetDateTime>,
    last_event_interest_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct MerchSnapshotRow {
    variant_id: Uuid,
    available_quantity: i64,
    sold_last_30d: i64,
    reorder_in_flight: bool,
    last_reorder_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct MerchPriceSnapshotRow {
    product_id: Uuid,
    current_price_minor: i64,
    minimum_price_minor: i64,
    maximum_price_minor: i64,
    unit_cost_minor: Option<i64>,
    economics_version: i64,
    available_quantity: i64,
    sold_last_7d: i64,
    sold_last_30d: i64,
    last_price_change_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct BookingSnapshotRow {
    city_id: Uuid,
    active_fans: i64,
    new_fans_30d: i64,
    event_interests: i64,
    area_claims: i64,
    months_since_last_show: Option<i64>,
    outreach_in_flight: bool,
    last_outreach_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct BookingTargetRow {
    target_id: Uuid,
    city_id: Uuid,
    target_kind: String,
    display_name: String,
    capacity: Option<i32>,
    version: i64,
    active: bool,
    accepts_booking: bool,
    priority: i32,
    relationship_score: i32,
    outreach_in_flight: bool,
    last_outreach_at: Option<OffsetDateTime>,
    followup_count: i32,
    last_reply_disposition: String,
}

#[derive(Debug, FromRow)]
struct MarketSignalRow {
    city_id: Uuid,
    signal_kind: String,
    score_basis_points: i32,
    confidence_basis_points: i32,
    observed_at: OffsetDateTime,
    expires_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct PromotionSnapshotRow {
    campaign_id: Uuid,
    current_daily_budget_minor: i64,
    minimum_daily_budget_minor: i64,
    maximum_daily_budget_minor: i64,
    spend_last_7d_minor: i64,
    attributed_revenue_last_7d_minor: i64,
    workspace_daily_budget_minor: i64,
    workspace_spend_month_to_date_minor: i64,
    workspace_maximum_daily_budget_minor: Option<i64>,
    workspace_maximum_monthly_spend_minor: Option<i64>,
    days_to_event: i64,
    active: bool,
    last_budget_change_at: Option<OffsetDateTime>,
    observed_at: OffsetDateTime,
    expires_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct ReleaseSnapshotRow {
    release_id: Uuid,
    title: String,
    release_at: OffsetDateTime,
    active: bool,
    assets_ready: bool,
    communication_enabled: bool,
    press_enabled: bool,
    calendar_seeded: bool,
    announcement_sent: bool,
    press_started: bool,
    fan_warmup_sent: bool,
    countdown_sent: bool,
    release_day_sent: bool,
    sustain_sent: bool,
    wrap_sent: bool,
}

#[derive(Debug, FromRow)]
struct TeamOpportunityRow {
    opportunity_id: Uuid,
    opportunity_kind: String,
    active: bool,
    verified_destination: bool,
    contact_email: Option<String>,
    metadata: Value,
    fit_basis_points: i32,
    reputation_basis_points: i32,
    confidence_basis_points: i32,
    expected_fee_minor: i64,
    estimated_cost_minor: i64,
    application_fee_minor: i64,
    requires_contract: bool,
    exclusive: bool,
    eligible: bool,
    funding_amount_minor: i64,
    own_contribution_minor: i64,
    deadline: Option<OffsetDateTime>,
    package_status: String,
    status: String,
}

#[derive(Debug, FromRow)]
struct ClaimedActionRow {
    id: Uuid,
    payload: Value,
    attempt_number: i32,
}

#[derive(Debug, FromRow)]
struct ClaimedMeasurementRow {
    id: Uuid,
    action_id: Uuid,
    measurement_kind: String,
    subject_id: Uuid,
    baseline_value: f64,
    action_finished_at: OffsetDateTime,
    attempt_number: i32,
}

#[derive(Debug, FromRow)]
struct PendingActionRow {
    id: Uuid,
    context: String,
    action_kind: String,
    subject_kind: String,
    subject_id: Uuid,
    payload: Value,
    created_at: OffsetDateTime,
    approval_expires_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct RecentDecisionRow {
    id: Uuid,
    context: String,
    decision_kind: String,
    confidence_basis_points: i32,
    disposition: String,
    reason: String,
    evaluated_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct RecentActionRow {
    id: Uuid,
    context: String,
    action_kind: String,
    subject_kind: String,
    subject_id: Uuid,
    status: String,
    attempt_count: i32,
    created_at: OffsetDateTime,
    finished_at: Option<OffsetDateTime>,
    last_error_kind: Option<String>,
    executor_status: Option<String>,
    executor_id: Option<String>,
    provider_reference: Option<String>,
    executor_reported_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct RecentEffectRow {
    measurement_id: Uuid,
    action_id: Uuid,
    context: String,
    measurement_kind: String,
    effect_assessment: String,
    delta_basis_points: i32,
    baseline_value: f64,
    observed_value: f64,
    observed_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct PromotionBudgetGuardrailRow {
    currency: String,
    maximum_total_daily_budget_minor: i64,
    maximum_monthly_spend_minor: i64,
    version: i64,
}

#[derive(Debug, FromRow)]
struct ControlStatsRow {
    queued_actions: i64,
    processing_actions: i64,
    succeeded_24h: i64,
    failed_24h: i64,
    executor_confirmed_24h: i64,
    executor_failed_24h: i64,
    awaiting_executor: i64,
}

#[derive(Debug, FromRow)]
struct ExistingOperatorActionRow {
    id: Uuid,
    action: String,
    target_type: String,
    target_id: Uuid,
    details: Value,
}

// decisions repository implementation lives in `autopilot/decisions.rs`.
// actions repository implementation lives in `autopilot/actions.rs`.
fn parse_policy(row: PolicyRow) -> Result<AutopilotPolicy, RepositoryError> {
    let context = match row.context.as_str() {
        "ticket_yield" => AutopilotContext::TicketYield,
        "fan_lifecycle" => AutopilotContext::FanLifecycle,
        "campaign_lifecycle" => AutopilotContext::CampaignLifecycle,
        "merchandising" => AutopilotContext::Merchandising,
        "merch_pricing" => AutopilotContext::MerchPricing,
        "merch_bundle" => AutopilotContext::MerchBundle,
        "booking_opportunity" => AutopilotContext::BookingOpportunity,
        "outreach" => AutopilotContext::Outreach,
        "content_supply" => AutopilotContext::ContentSupply,
        "promotion_budget" => AutopilotContext::PromotionBudget,
        "experimentation" => AutopilotContext::Experimentation,
        "show_operations" => AutopilotContext::ShowOperations,
        "release" => AutopilotContext::Release,
        "live_opportunity" => AutopilotContext::LiveOpportunity,
        "funding" => AutopilotContext::Funding,
        _ => return Err(RepositoryError::Unexpected),
    };
    let autonomy_level = match row.autonomy_level.as_str() {
        "observe" => AutonomyLevel::Observe,
        "recommend" => AutonomyLevel::Recommend,
        "require_approval" => AutonomyLevel::RequireApproval,
        "bounded_auto" => AutonomyLevel::BoundedAuto,
        _ => return Err(RepositoryError::Unexpected),
    };
    let confidence = u16::try_from(row.minimum_confidence_basis_points)
        .ok()
        .and_then(|value| Confidence::from_basis_points(value).ok())
        .ok_or(RepositoryError::Unexpected)?;
    let config =
        match context {
            AutopilotContext::TicketYield => AutopilotPolicyConfig::TicketYield(parse_config(
                row.config,
                TicketYieldPolicy::default(),
            )?),
            AutopilotContext::FanLifecycle => AutopilotPolicyConfig::FanLifecycle(parse_config(
                row.config,
                FanLifecyclePolicy::default(),
            )?),
            AutopilotContext::CampaignLifecycle => AutopilotPolicyConfig::CampaignLifecycle(
                parse_config(row.config, EventCampaignPolicy::default())?,
            ),
            AutopilotContext::Merchandising => AutopilotPolicyConfig::Merchandising(parse_config(
                row.config,
                MerchReorderPolicy::default(),
            )?),
            AutopilotContext::MerchPricing => AutopilotPolicyConfig::MerchPricing(parse_config(
                row.config,
                MerchPricePolicy::default(),
            )?),
            AutopilotContext::MerchBundle => AutopilotPolicyConfig::MerchBundle(parse_config(
                row.config,
                MerchBundlePolicy::default(),
            )?),
            AutopilotContext::BookingOpportunity => AutopilotPolicyConfig::BookingOpportunity(
                parse_config(row.config, BookingOpportunityPolicy::default())?,
            ),
            AutopilotContext::Outreach => AutopilotPolicyConfig::Outreach(parse_config(
                row.config,
                OutreachPolicy::default(),
            )?),
            AutopilotContext::ContentSupply => AutopilotPolicyConfig::ContentSupply(parse_config(
                row.config,
                ContentSupplyPolicy::default(),
            )?),
            AutopilotContext::PromotionBudget => AutopilotPolicyConfig::PromotionBudget(
                parse_config(row.config, PromotionBudgetPolicy::default())?,
            ),
            AutopilotContext::Experimentation => AutopilotPolicyConfig::Experimentation(
                parse_config(row.config, ExperimentPolicy::default())?,
            ),
            AutopilotContext::ShowOperations => AutopilotPolicyConfig::ShowOperations(
                parse_config(row.config, ShowOperationsPolicy::default())?,
            ),
            AutopilotContext::Release => AutopilotPolicyConfig::Release(parse_config(
                row.config,
                ReleaseAutopilotPolicy::default(),
            )?),
            AutopilotContext::LiveOpportunity => AutopilotPolicyConfig::LiveOpportunity(
                parse_config(row.config, LiveOpportunityPolicy::default())?,
            ),
            AutopilotContext::Funding => {
                AutopilotPolicyConfig::Funding(parse_config(row.config, FundingPolicy::default())?)
            }
        };
    let max_actions_24h = u32::try_from(row.max_actions_24h)
        .ok()
        .filter(|value| *value > 0)
        .ok_or(RepositoryError::Unexpected)?;
    Ok(AutopilotPolicy {
        context,
        enabled: row.enabled,
        autonomy_level,
        minimum_confidence: confidence,
        max_actions_24h,
        config,
        version: row.version,
        guarded_until: row.guarded_until,
        guardrail_reason: row.guardrail_reason,
    })
}

fn parse_config<T>(value: Value, default: T) -> Result<T, RepositoryError>
where
    T: serde::de::DeserializeOwned,
{
    if value.as_object().is_some_and(serde_json::Map::is_empty) {
        Ok(default)
    } else {
        serde_json::from_value(value).map_err(|_| RepositoryError::Unexpected)
    }
}

fn ticket_snapshot(row: TicketSnapshotRow) -> Result<TicketYieldSnapshot, RepositoryError> {
    Ok(TicketYieldSnapshot {
        ticket_type_id: TicketTypeId::from_uuid(row.ticket_type_id),
        current_price_minor: row.current_price_minor,
        paid_quantity: bounded_u32(row.paid_quantity)?,
        capacity: bounded_u32(row.capacity)?,
        sale_capacity: bounded_u32(row.sale_capacity)?,
        paid_last_72h: bounded_u32(row.paid_last_72h)?,
        days_to_event: bounded_u32(row.days_to_event)?,
        last_price_change_at: row.last_price_change_at,
        last_capacity_change_at: row.last_capacity_change_at,
        allocation_guardrail: match (
            row.allocation_minimum_capacity,
            row.allocation_maximum_capacity,
            row.allocation_step_capacity,
            row.allocation_guardrail_version,
        ) {
            (Some(minimum), Some(maximum), Some(step), Some(version)) => {
                Some(crowdrelay_domain::pricing::TicketAllocationGuardrail {
                    minimum_capacity: bounded_u32(i64::from(minimum))?,
                    maximum_capacity: bounded_u32(i64::from(maximum))?,
                    step_capacity: bounded_u32(i64::from(step))?,
                    version,
                })
            }
            (None, None, None, None) => None,
            _ => return Err(RepositoryError::Unexpected),
        },
    })
}

fn lifecycle_snapshot(row: LifecycleSnapshotRow) -> Result<FanLifecycleSnapshot, RepositoryError> {
    Ok(FanLifecycleSnapshot {
        fan_id: FanId::from_uuid(row.fan_id),
        active: row.active,
        marketing_consent: row.marketing_consent,
        created_at: row.created_at,
        synesthesia_completed_at: row.synesthesia_completed_at,
        last_marketing_touch_at: row.last_marketing_touch_at,
        has_paid_ticket: row.has_paid_ticket,
        last_paid_ticket_at: row.last_paid_ticket_at,
        last_event_interest_at: row.last_event_interest_at,
    })
}

fn merch_snapshot(row: MerchSnapshotRow) -> Result<MerchInventorySnapshot, RepositoryError> {
    Ok(MerchInventorySnapshot {
        variant_id: MerchVariantId::from_uuid(row.variant_id),
        available_quantity: bounded_u32(row.available_quantity)?,
        sold_last_30d: bounded_u32(row.sold_last_30d)?,
        reorder_in_flight: row.reorder_in_flight,
        last_reorder_at: row.last_reorder_at,
    })
}

fn merch_price_snapshot(row: MerchPriceSnapshotRow) -> Result<MerchPriceSnapshot, RepositoryError> {
    Ok(MerchPriceSnapshot {
        product_id: MerchProductId::from_uuid(row.product_id),
        current_price_minor: bounded_u64(row.current_price_minor)?,
        minimum_price_minor: bounded_u64(row.minimum_price_minor)?,
        maximum_price_minor: bounded_u64(row.maximum_price_minor)?,
        unit_cost_minor: row.unit_cost_minor.map(bounded_u64).transpose()?,
        economics_version: row.economics_version,
        available_quantity: bounded_u32(row.available_quantity)?,
        sold_last_7d: bounded_u32(row.sold_last_7d)?,
        sold_last_30d: bounded_u32(row.sold_last_30d)?,
        last_price_change_at: row.last_price_change_at,
    })
}

fn booking_snapshot(
    row: BookingSnapshotRow,
    market_evidence: Option<crowdrelay_domain::market_intelligence::CityMarketEvidence>,
) -> Result<CityOpportunitySnapshot, RepositoryError> {
    Ok(CityOpportunitySnapshot {
        city_id: CityId::from_uuid(row.city_id),
        active_fans: bounded_u32(row.active_fans)?,
        new_fans_30d: bounded_u32(row.new_fans_30d)?,
        event_interests: bounded_u32(row.event_interests)?,
        area_claims: bounded_u32(row.area_claims)?,
        months_since_last_show: row.months_since_last_show.map(bounded_u32).transpose()?,
        market_evidence,
        outreach_in_flight: row.outreach_in_flight,
        last_outreach_at: row.last_outreach_at,
    })
}

fn booking_target_snapshot(
    row: BookingTargetRow,
) -> Result<BookingTargetSnapshot, RepositoryError> {
    Ok(BookingTargetSnapshot {
        target_id: BookingTargetId::from_uuid(row.target_id),
        city_id: CityId::from_uuid(row.city_id),
        kind: parse_booking_target_kind(&row.target_kind)?,
        display_name: row.display_name,
        capacity: row
            .capacity
            .map(|value| bounded_u32(i64::from(value)))
            .transpose()?,
        version: row.version,
        active: row.active,
        accepts_booking: row.accepts_booking,
        priority: u16::try_from(row.priority).map_err(|_| RepositoryError::Unexpected)?,
        relationship_score: u16::try_from(row.relationship_score)
            .map_err(|_| RepositoryError::Unexpected)?,
        outreach_in_flight: row.outreach_in_flight,
        last_outreach_at: row.last_outreach_at,
        followup_count: u16::try_from(row.followup_count)
            .map_err(|_| RepositoryError::Unexpected)?,
        last_reply: parse_booking_reply_disposition(&row.last_reply_disposition)?,
    })
}

fn market_signal(row: MarketSignalRow) -> Result<CityMarketSignal, RepositoryError> {
    Ok(CityMarketSignal {
        kind: parse_market_signal_kind(&row.signal_kind)?,
        score_basis_points: u16::try_from(row.score_basis_points)
            .ok()
            .filter(|value| *value <= 10_000)
            .ok_or(RepositoryError::Unexpected)?,
        confidence: parse_confidence(row.confidence_basis_points)?,
        observed_at: row.observed_at,
        expires_at: row.expires_at,
    })
}

fn promotion_snapshot(
    row: PromotionSnapshotRow,
) -> Result<PromotionPerformanceSnapshot, RepositoryError> {
    Ok(PromotionPerformanceSnapshot {
        campaign_id: PromotionCampaignId::from_uuid(row.campaign_id),
        current_daily_budget_minor: row.current_daily_budget_minor,
        minimum_daily_budget_minor: row.minimum_daily_budget_minor,
        maximum_daily_budget_minor: row.maximum_daily_budget_minor,
        spend_last_7d_minor: row.spend_last_7d_minor,
        attributed_revenue_last_7d_minor: row.attributed_revenue_last_7d_minor,
        workspace_daily_budget_minor: row.workspace_daily_budget_minor,
        workspace_spend_month_to_date_minor: row.workspace_spend_month_to_date_minor,
        workspace_maximum_daily_budget_minor: row.workspace_maximum_daily_budget_minor,
        workspace_maximum_monthly_spend_minor: row.workspace_maximum_monthly_spend_minor,
        days_to_event: bounded_u32(row.days_to_event)?,
        active: row.active,
        last_budget_change_at: row.last_budget_change_at,
        observed_at: row.observed_at,
        expires_at: row.expires_at,
    })
}

fn bounded_u32(value: i64) -> Result<u32, RepositoryError> {
    u32::try_from(value).map_err(|_| RepositoryError::Unexpected)
}

fn bounded_u64(value: i64) -> Result<u64, RepositoryError> {
    u64::try_from(value).map_err(|_| RepositoryError::Unexpected)
}

const fn disposition_str(disposition: PolicyDisposition) -> &'static str {
    match disposition {
        PolicyDisposition::ObserveOnly => "observe_only",
        PolicyDisposition::RecommendOnly => "recommend_only",
        PolicyDisposition::RequireApproval => "require_approval",
        PolicyDisposition::AutoExecute => "auto_execute",
        PolicyDisposition::Deny => "deny",
    }
}

include!("autopilot/execution.rs");
async fn configure_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    operation_timeout: Duration,
    lock_timeout: Duration,
) -> Result<(), RepositoryError> {
    let statement_ms =
        u64::try_from(operation_timeout.as_millis()).map_err(|_| RepositoryError::Unexpected)?;
    let lock_ms =
        u64::try_from(lock_timeout.as_millis()).map_err(|_| RepositoryError::Unexpected)?;
    sqlx::query(
        "SELECT set_config('statement_timeout', $1, true), set_config('lock_timeout', $2, true)",
    )
    .bind(format!("{statement_ms}ms"))
    .bind(format!("{lock_ms}ms"))
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

fn map_sqlx(error: sqlx::Error) -> RepositoryError {
    match classify_sqlx_error(&error) {
        SqlxErrorClass::NotFound => RepositoryError::NotFound,
        SqlxErrorClass::Conflict => RepositoryError::Conflict,
        SqlxErrorClass::Unavailable => RepositoryError::Unavailable,
        SqlxErrorClass::Unexpected => RepositoryError::Unexpected,
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    #[test]
    fn policy_config_defaults_when_database_json_is_empty() {
        let row = PolicyRow {
            context: "ticket_yield".to_owned(),
            enabled: false,
            autonomy_level: "observe".to_owned(),
            minimum_confidence_basis_points: 8_000,
            max_actions_24h: 10,
            config: json!({}),
            version: 1,
            guarded_until: None,
            guardrail_reason: None,
        };
        let result = parse_policy(row);
        assert!(result.is_ok());
        if let Ok(policy) = result {
            assert_eq!(policy.context, AutopilotContext::TicketYield);
            assert!(matches!(
                policy.config,
                AutopilotPolicyConfig::TicketYield(TicketYieldPolicy {
                    step_minor: 500,
                    ..
                })
            ));
            assert_eq!(policy.version, 1);
        }
    }
}

// measurement repository implementation lives in `autopilot/measurement.rs`.
// state repository implementation lives in `autopilot/state.rs`.
// control repository implementation lives in `autopilot/control.rs`.
fn policy_summary(row: PolicyRow) -> Result<AutopilotPolicySummary, RepositoryError> {
    Ok(AutopilotPolicySummary {
        context: parse_context(&row.context)?,
        enabled: row.enabled,
        autonomy_level: parse_autonomy_level(&row.autonomy_level)?,
        minimum_confidence: parse_confidence(row.minimum_confidence_basis_points)?,
        max_actions_24h: u32::try_from(row.max_actions_24h)
            .map_err(|_| RepositoryError::Unexpected)?,
        version: row.version,
        guarded_until: row.guarded_until,
        guardrail_reason: row.guardrail_reason,
    })
}

fn pending_action(row: PendingActionRow) -> Result<PendingAutopilotAction, RepositoryError> {
    Ok(PendingAutopilotAction {
        id: AutopilotActionId::from_uuid(row.id),
        context: parse_context(&row.context)?,
        action_kind: row.action_kind,
        subject_kind: row.subject_kind,
        subject_id: row.subject_id,
        payload: serde_json::from_value(row.payload).map_err(|_| RepositoryError::Unexpected)?,
        created_at: row.created_at,
        approval_expires_at: row.approval_expires_at,
    })
}

fn recent_action(row: RecentActionRow) -> Result<RecentAutopilotAction, RepositoryError> {
    Ok(RecentAutopilotAction {
        id: AutopilotActionId::from_uuid(row.id),
        context: parse_context(&row.context)?,
        action_kind: row.action_kind,
        subject_kind: row.subject_kind,
        subject_id: row.subject_id,
        status: row.status,
        attempt_count: u32::try_from(row.attempt_count).map_err(|_| RepositoryError::Unexpected)?,
        created_at: row.created_at,
        finished_at: row.finished_at,
        last_error_kind: row.last_error_kind,
        executor_status: row.executor_status,
        executor_id: row.executor_id,
        provider_reference: row.provider_reference,
        executor_reported_at: row.executor_reported_at,
    })
}

fn recent_effect(row: RecentEffectRow) -> Result<RecentAutopilotEffect, RepositoryError> {
    Ok(RecentAutopilotEffect {
        measurement_id: AutopilotMeasurementId::from_uuid(row.measurement_id),
        action_id: AutopilotActionId::from_uuid(row.action_id),
        context: parse_context(&row.context)?,
        measurement_kind: parse_measurement_kind(&row.measurement_kind)?,
        assessment: parse_effect_assessment(&row.effect_assessment)?,
        delta_basis_points: row.delta_basis_points,
        baseline_value: row.baseline_value,
        observed_value: row.observed_value,
        observed_at: row.observed_at,
    })
}

fn recent_decision(row: RecentDecisionRow) -> Result<RecentAutopilotDecision, RepositoryError> {
    Ok(RecentAutopilotDecision {
        id: AutopilotDecisionId::from_uuid(row.id),
        context: parse_context(&row.context)?,
        decision_kind: row.decision_kind,
        confidence: parse_confidence(row.confidence_basis_points)?,
        disposition: parse_disposition(&row.disposition)?,
        reason: row.reason,
        evaluated_at: row.evaluated_at,
    })
}

fn claimed_measurement(
    row: ClaimedMeasurementRow,
) -> Result<ClaimedAutopilotMeasurement, RepositoryError> {
    Ok(ClaimedAutopilotMeasurement {
        id: AutopilotMeasurementId::from_uuid(row.id),
        action_id: AutopilotActionId::from_uuid(row.action_id),
        kind: parse_measurement_kind(&row.measurement_kind)?,
        subject_id: row.subject_id,
        baseline_value: row.baseline_value,
        action_finished_at: row.action_finished_at,
        attempt_number: u32::try_from(row.attempt_number)
            .map_err(|_| RepositoryError::Unexpected)?,
    })
}

fn parse_measurement_kind(value: &str) -> Result<AutopilotMeasurementKind, RepositoryError> {
    match value {
        "ticket_revenue_72h" => Ok(AutopilotMeasurementKind::TicketRevenue72h),
        "merch_gross_proxy_7d" => Ok(AutopilotMeasurementKind::MerchGrossProxy7d),
        "promotion_roas_7d" => Ok(AutopilotMeasurementKind::PromotionRoas7d),
        _ => Err(RepositoryError::Unexpected),
    }
}

fn parse_effect_assessment(value: &str) -> Result<EffectAssessment, RepositoryError> {
    match value {
        "improved" => Ok(EffectAssessment::Improved),
        "neutral" => Ok(EffectAssessment::Neutral),
        "worsened" => Ok(EffectAssessment::Worsened),
        _ => Err(RepositoryError::Unexpected),
    }
}

const fn effect_assessment_str(value: EffectAssessment) -> &'static str {
    match value {
        EffectAssessment::Improved => "improved",
        EffectAssessment::Neutral => "neutral",
        EffectAssessment::Worsened => "worsened",
    }
}

fn parse_booking_reply_disposition(
    value: &str,
) -> Result<BookingReplyDisposition, RepositoryError> {
    match value {
        "none" => Ok(BookingReplyDisposition::None),
        "received" => Ok(BookingReplyDisposition::Received),
        "positive" => Ok(BookingReplyDisposition::Positive),
        "declined" => Ok(BookingReplyDisposition::Declined),
        "booked" => Ok(BookingReplyDisposition::Booked),
        "do_not_contact" => Ok(BookingReplyDisposition::DoNotContact),
        _ => Err(RepositoryError::Unexpected),
    }
}

fn parse_booking_target_kind(value: &str) -> Result<BookingTargetKind, RepositoryError> {
    match value {
        "venue" => Ok(BookingTargetKind::Venue),
        "promoter" => Ok(BookingTargetKind::Promoter),
        "festival" => Ok(BookingTargetKind::Festival),
        _ => Err(RepositoryError::Unexpected),
    }
}

fn parse_market_signal_kind(value: &str) -> Result<CityMarketSignalKind, RepositoryError> {
    match value {
        "streaming_momentum" => Ok(CityMarketSignalKind::StreamingMomentum),
        "search_interest" => Ok(CityMarketSignalKind::SearchInterest),
        "social_momentum" => Ok(CityMarketSignalKind::SocialMomentum),
        "live_demand" => Ok(CityMarketSignalKind::LiveDemand),
        _ => Err(RepositoryError::Unexpected),
    }
}

fn parse_context(value: &str) -> Result<AutopilotContext, RepositoryError> {
    match value {
        "ticket_yield" => Ok(AutopilotContext::TicketYield),
        "fan_lifecycle" => Ok(AutopilotContext::FanLifecycle),
        "campaign_lifecycle" => Ok(AutopilotContext::CampaignLifecycle),
        "merchandising" => Ok(AutopilotContext::Merchandising),
        "merch_pricing" => Ok(AutopilotContext::MerchPricing),
        "merch_bundle" => Ok(AutopilotContext::MerchBundle),
        "booking_opportunity" => Ok(AutopilotContext::BookingOpportunity),
        "outreach" => Ok(AutopilotContext::Outreach),
        "content_supply" => Ok(AutopilotContext::ContentSupply),
        "promotion_budget" => Ok(AutopilotContext::PromotionBudget),
        "experimentation" => Ok(AutopilotContext::Experimentation),
        "show_operations" => Ok(AutopilotContext::ShowOperations),
        "release" => Ok(AutopilotContext::Release),
        "live_opportunity" => Ok(AutopilotContext::LiveOpportunity),
        "funding" => Ok(AutopilotContext::Funding),
        _ => Err(RepositoryError::Unexpected),
    }
}

fn parse_autonomy_level(value: &str) -> Result<AutonomyLevel, RepositoryError> {
    match value {
        "observe" => Ok(AutonomyLevel::Observe),
        "recommend" => Ok(AutonomyLevel::Recommend),
        "require_approval" => Ok(AutonomyLevel::RequireApproval),
        "bounded_auto" => Ok(AutonomyLevel::BoundedAuto),
        _ => Err(RepositoryError::Unexpected),
    }
}

fn parse_confidence(value: i32) -> Result<Confidence, RepositoryError> {
    u16::try_from(value)
        .ok()
        .and_then(|basis_points| Confidence::from_basis_points(basis_points).ok())
        .ok_or(RepositoryError::Unexpected)
}

fn parse_disposition(value: &str) -> Result<PolicyDisposition, RepositoryError> {
    match value {
        "observe_only" => Ok(PolicyDisposition::ObserveOnly),
        "recommend_only" => Ok(PolicyDisposition::RecommendOnly),
        "require_approval" => Ok(PolicyDisposition::RequireApproval),
        "auto_execute" => Ok(PolicyDisposition::AutoExecute),
        "deny" => Ok(PolicyDisposition::Deny),
        _ => Err(RepositoryError::Unexpected),
    }
}

const fn autonomy_level_str(value: AutonomyLevel) -> &'static str {
    match value {
        AutonomyLevel::Observe => "observe",
        AutonomyLevel::Recommend => "recommend",
        AutonomyLevel::RequireApproval => "require_approval",
        AutonomyLevel::BoundedAuto => "bounded_auto",
    }
}
