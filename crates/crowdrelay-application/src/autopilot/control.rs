//! Operator control plane and provider-neutral state ingress ports.

use async_trait::async_trait;
use crowdrelay_domain::{
    AutopilotActionId, AutopilotMeasurementId, BeaconId, BookingTargetId, CityId, ContentSourceId,
    EventId, ExperimentId, ExperimentVariantId, MarketSignalId, MerchProductId,
    OutreachOpportunityId, OutreachTargetId, PromotionCampaignId, ReleasePlanId, TeamOpportunityId,
    WorkspaceId,
    autonomy::{AutonomyLevel, Confidence, PolicyDisposition},
    beacons::{BeaconKind, BeaconReplyDisposition},
    booking::{BookingReplyDisposition, BookingTargetKind},
    content_supply::ContentSourceKind,
    experimentation::{ExperimentAllocationSlot, ExperimentMetric, assign_variant},
    live_opportunities::{BookingManagerPolicy, LiveTravelBand},
    market_intelligence::CityMarketSignalKind,
    outreach::{OutreachReplyDisposition, OutreachTargetKind},
};
use serde::{Deserialize, Serialize, Serializer};
use time::OffsetDateTime;

use super::{
    model::{AutopilotActionPayload, AutopilotContext},
    ports::AutopilotMeasurementKind,
};
use crate::{IdempotencyKey, RepositoryError, RequestId};

#[derive(Clone, Debug, Serialize)]
pub struct AutopilotPolicySummary {
    pub context: AutopilotContext,
    pub enabled: bool,
    pub autonomy_level: AutonomyLevel,
    pub minimum_confidence: Confidence,
    pub max_actions_24h: u32,
    pub version: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub guarded_until: Option<OffsetDateTime>,
    pub guardrail_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PromotionBudgetGuardrailSummary {
    pub currency: String,
    pub maximum_total_daily_budget_minor: i64,
    pub maximum_monthly_spend_minor: i64,
    pub version: i64,
}

/// Non-sensitive owner shown consistently across staff surfaces. Contact email
/// stays private in the notification executor payload.
#[derive(Clone, Debug, Serialize)]
pub struct TeamAssigneeSummary {
    pub member_id: uuid::Uuid,
    pub member_key: String,
    pub display_name: String,
}

fn serialize_control_payload<S>(
    payload: &AutopilotActionPayload,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut value = serde_json::to_value(payload).map_err(serde::ser::Error::custom)?;
    let object = value.as_object_mut().ok_or_else(|| {
        serde::ser::Error::custom("autopilot payload must serialize as an object")
    })?;

    // The durable action enum intentionally retains its original Serde shape so
    // historical JSONB rows remain readable. The operator API is a separate
    // boundary: date-times are RFC3339 strings and executor-only recipient PII
    // is never exposed to Signal or another control-plane client.
    match payload {
        AutopilotActionPayload::ExecuteReleaseMilestone { release_at, .. } => {
            let formatted = release_at
                .format(&time::format_description::well_known::Rfc3339)
                .map_err(serde::ser::Error::custom)?;
            object.insert(
                "release_at".to_owned(),
                serde_json::Value::String(formatted),
            );
        }
        AutopilotActionPayload::SendTeamAssignmentEmail { due_at, .. } => {
            object.remove("recipient_email");
            let formatted = (*due_at)
                .map(|value| value.format(&time::format_description::well_known::Rfc3339))
                .transpose()
                .map_err(serde::ser::Error::custom)?;
            object.insert(
                "due_at".to_owned(),
                formatted.map_or(serde_json::Value::Null, serde_json::Value::String),
            );
        }
        _ => {}
    }
    value.serialize(serializer)
}

/// Human-actionable Autopilot job waiting in the approval queue.
#[derive(Clone, Debug, Serialize)]
pub struct PendingAutopilotAction {
    pub id: AutopilotActionId,
    pub context: AutopilotContext,
    pub action_kind: String,
    pub subject_kind: String,
    pub subject_id: uuid::Uuid,
    #[serde(serialize_with = "serialize_control_payload")]
    pub payload: AutopilotActionPayload,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub approval_expires_at: Option<OffsetDateTime>,
    pub assignee: Option<TeamAssigneeSummary>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub assignment_due_at: Option<OffsetDateTime>,
}

/// Compact immutable decision trail shown in the operator cockpit.
#[derive(Clone, Debug, Serialize)]
pub struct RecentAutopilotDecision {
    pub id: crowdrelay_domain::AutopilotDecisionId,
    pub context: AutopilotContext,
    pub decision_kind: String,
    pub confidence: Confidence,
    pub disposition: PolicyDisposition,
    pub reason: String,
    #[serde(with = "time::serde::rfc3339")]
    pub evaluated_at: OffsetDateTime,
}

/// A bounded human-only step returned by an executor when a free distribution
/// surface cannot be automated safely (for example CAPTCHA/login verification).
/// Only public destination data is exposed to staff surfaces; arbitrary executor
/// metadata remains private to the execution ledger.
#[derive(Clone, Debug, Serialize)]
pub struct AutopilotManualStep {
    pub destination: String,
    pub url: String,
    pub what_to_do: String,
    pub why_it_matters: String,
}

/// Recent action execution evidence. This is deliberately compact and contains
/// no recipient PII; the cockpit is an exception surface, not a data browser.
#[derive(Clone, Debug, Serialize)]
pub struct RecentAutopilotAction {
    pub id: AutopilotActionId,
    pub context: AutopilotContext,
    pub action_kind: String,
    pub subject_kind: String,
    pub subject_id: uuid::Uuid,
    pub status: String,
    pub attempt_count: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    pub last_error_kind: Option<String>,
    pub executor_status: Option<String>,
    pub executor_id: Option<String>,
    pub provider_reference: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub executor_reported_at: Option<OffsetDateTime>,
    pub manual_steps: Vec<AutopilotManualStep>,
}

/// A delayed, measured effect of one successfully executed Autopilot action.
/// This is evidence for calibration, not proof of causality; the metric name is
/// intentionally explicit about proxies where exact attribution is unavailable.
#[derive(Clone, Debug, Serialize)]
pub struct RecentAutopilotEffect {
    pub measurement_id: AutopilotMeasurementId,
    pub action_id: AutopilotActionId,
    pub context: AutopilotContext,
    pub measurement_kind: AutopilotMeasurementKind,
    pub assessment: crowdrelay_domain::performance::EffectAssessment,
    pub delta_basis_points: i32,
    pub baseline_value: f64,
    pub observed_value: f64,
    #[serde(with = "time::serde::rfc3339")]
    pub observed_at: OffsetDateTime,
}

/// Exception-first operator view. The UI should emphasize `needs_you`; recent
/// decisions/actions and measured effects exist as evidence, not as another dashboard
/// operators must watch.
#[derive(Clone, Debug, Serialize)]
pub struct RumMetricSummary {
    pub surface: String,
    pub metric_key: String,
    pub samples_24h: i64,
    pub p75: f64,
    pub p95: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct AutopilotControlOverview {
    pub policies: Vec<AutopilotPolicySummary>,
    pub promotion_budget_guardrails: Vec<PromotionBudgetGuardrailSummary>,
    pub needs_you: Vec<PendingAutopilotAction>,
    /// Active human owners eligible for an explicit operator re-assignment.
    /// No contact PII is exposed on staff read models.
    pub available_assignees: Vec<TeamAssigneeSummary>,
    pub recent_decisions: Vec<RecentAutopilotDecision>,
    pub recent_actions: Vec<RecentAutopilotAction>,
    pub recent_effects: Vec<RecentAutopilotEffect>,
    pub queued_actions: i64,
    pub processing_actions: i64,
    pub succeeded_24h: i64,
    pub failed_24h: i64,
    pub executor_confirmed_24h: i64,
    pub executor_failed_24h: i64,
    pub awaiting_executor: i64,
    pub release_ledger: ReleaseLedgerOverview,
    pub rum_metrics_24h: Vec<RumMetricSummary>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChiefOfStaffOpportunity {
    pub context: AutopilotContext,
    pub decision_kind: String,
    pub subject_kind: String,
    pub subject_id: uuid::Uuid,
    pub confidence: Confidence,
    pub reason: String,
    pub needs_approval: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChiefOfStaffShowTask {
    pub event_id: EventId,
    pub event_title: String,
    pub task_key: String,
    pub status: String,
    #[serde(with = "time::serde::rfc3339")]
    pub starts_at: OffsetDateTime,
}

/// Time-sensitive fact surfaced by the Chief-of-Staff read model. This does
/// not create another task system: approvals and team-opportunity deadlines
/// remain owned by their existing bounded contexts.
#[derive(Clone, Debug, Serialize)]
pub struct ChiefOfStaffAttentionItem {
    pub kind: String,
    pub subject_kind: String,
    pub subject_id: uuid::Uuid,
    pub title: String,
    pub detail: String,
    #[serde(with = "time::serde::rfc3339")]
    pub due_at: OffsetDateTime,
    pub urgency: String,
}

/// Deterministic daily operating brief. `estimated_minutes_saved_24h` is a
/// coarse action-kind workload estimate, not fabricated AI precision.
#[derive(Clone, Debug, Serialize)]
pub struct AutopilotChiefOfStaff {
    pub executed_24h: i64,
    pub failed_24h: i64,
    pub needs_you: i64,
    pub estimated_minutes_saved_24h: i64,
    pub measured_improved_7d: i64,
    pub measured_neutral_7d: i64,
    pub measured_worsened_7d: i64,
    pub emitted_24h: i64,
    pub executor_confirmed_24h: i64,
    pub executor_failed_24h: i64,
    pub attention_items: Vec<ChiefOfStaffAttentionItem>,
    pub top_opportunities: Vec<ChiefOfStaffOpportunity>,
    pub show_tasks: Vec<ChiefOfStaffShowTask>,
}

/// Authority-only policy mutation. Domain-specific thresholds remain typed
/// config owned by code/migrations until a dedicated validated editor exists.
#[derive(Clone, Copy, Debug)]
pub struct SetAutopilotAuthority {
    pub context: AutopilotContext,
    pub enabled: bool,
    pub autonomy_level: AutonomyLevel,
    pub minimum_confidence: Confidence,
    pub max_actions_24h: u32,
    /// Optimistic-concurrency guard from the overview read model.
    pub expected_version: i64,
}

/// Result of an audited operator mutation.
#[derive(Clone, Debug, Serialize)]
pub struct AutopilotControlMutation {
    pub operation_id: uuid::Uuid,
    pub target_id: uuid::Uuid,
    pub status: String,
    pub replayed: bool,
}

include!("control/state_ports.rs");
include!("control/runtime_ports.rs");
