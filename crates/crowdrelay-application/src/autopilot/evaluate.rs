//! Thin orchestration from typed snapshots to durable decision candidates.

use crowdrelay_domain::{
    WorkspaceId,
    audience_lifecycle::{
        FanLifecycleDecision, FanLifecycleSnapshot, LifecycleTemplate, evaluate_fan_lifecycle,
    },
    autonomy::{PolicyDisposition, disposition},
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

fn policy_evidence<T: Serialize>(
    policy: AutopilotPolicy,
    domain_config: T,
) -> Result<serde_json::Value, serde_json::Error> {
    Ok(serde_json::json!({
        "version": policy.version,
        "enabled": policy.enabled,
        "autonomy_level": policy.autonomy_level,
        "minimum_confidence_basis_points": policy.minimum_confidence.basis_points(),
        "max_actions_24h": policy.max_actions_24h,
        "config": serde_json::to_value(domain_config)?,
    }))
}

fn ticket_candidate(
    snapshot: TicketYieldSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::TicketYield(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let TicketYieldDecision::Increase {
        from_minor,
        to_minor,
        confidence,
    } = evaluate_ticket_yield(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let subject = ActionSubject::TicketType(snapshot.ticket_type_id);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject,
        decision_kind: "increase_ticket_price",
        confidence,
        disposition,
        reason: "paid demand exceeds bounded yield thresholds",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy.clone(), domain_policy)?,
        action: AutopilotActionPayload::ChangeTicketPrice {
            ticket_type_id: snapshot.ticket_type_id,
            from_minor,
            to_minor,
        },
        decision_key: format!(
            "decision:ticket:v{}:{}:{}:{}:{}:{}:{}:{from_minor}:{to_minor}",
            policy.version,
            snapshot.ticket_type_id,
            snapshot.current_price_minor,
            snapshot.paid_quantity,
            snapshot.capacity,
            snapshot.paid_last_72h,
            snapshot.days_to_event,
        ),
        action_idempotency_key: format!(
            "action:ticket:{}:{}:{from_minor}:{to_minor}",
            snapshot.ticket_type_id,
            snapshot.last_price_change_at.map_or_else(
                || "initial".to_owned(),
                |at| at.unix_timestamp().to_string(),
            )
        ),
    }))
}

fn ticket_allocation_candidate(
    snapshot: TicketYieldSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::TicketYield(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let TicketAllocationDecision::IncreaseCapacity {
        from_capacity,
        to_capacity,
        guardrail_version,
        confidence,
    } = evaluate_ticket_allocation(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let subject = ActionSubject::TicketType(snapshot.ticket_type_id);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject,
        decision_kind: "increase_ticket_capacity",
        confidence,
        disposition,
        reason: "paid tier demand is near its operator-bounded allocation ceiling",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy.clone(), domain_policy)?,
        action: AutopilotActionPayload::ChangeTicketCapacity {
            ticket_type_id: snapshot.ticket_type_id,
            from_capacity,
            to_capacity,
            guardrail_version,
        },
        decision_key: format!(
            "decision:ticket-capacity:v{}:{}:g{}:{}:{}:{}:{}:{from_capacity}:{to_capacity}",
            policy.version,
            snapshot.ticket_type_id,
            guardrail_version,
            snapshot.paid_quantity,
            snapshot.paid_last_72h,
            snapshot.days_to_event,
            snapshot.sale_capacity,
        ),
        action_idempotency_key: format!(
            "action:ticket-capacity:{}:g{}:{}:{from_capacity}:{to_capacity}",
            snapshot.ticket_type_id,
            guardrail_version,
            snapshot.last_capacity_change_at.map_or_else(
                || "initial".to_owned(),
                |at| at.unix_timestamp().to_string(),
            ),
        ),
    }))
}

fn lifecycle_candidate(
    snapshot: FanLifecycleSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::FanLifecycle(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let FanLifecycleDecision::RequestMessage {
        template,
        confidence,
    } = evaluate_fan_lifecycle(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let template_key = match template {
        LifecycleTemplate::Welcome => "viryaos.fan.welcome.v1",
        LifecycleTemplate::SynesthesiaFollowUp => "viryaos.synesthesia.follow_up.v1",
        LifecycleTemplate::DormantReactivation => "viryaos.fan.reactivation.v1",
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let subject = ActionSubject::Fan(snapshot.fan_id);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject,
        decision_kind: "request_lifecycle_message",
        confidence,
        disposition,
        reason: "consented fan lifecycle has a deterministic communication step due",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy.clone(), domain_policy)?,
        action: AutopilotActionPayload::RequestFanLifecycleMessage {
            fan_id: snapshot.fan_id,
            template_key: template_key.to_owned(),
        },
        decision_key: format!(
            "decision:lifecycle:v{}:{}:{}:{}:{}",
            policy.version,
            snapshot.fan_id,
            template_key,
            snapshot
                .last_marketing_touch_at
                .map_or(0, OffsetDateTime::unix_timestamp),
            snapshot
                .last_event_interest_at
                .map_or(0, OffsetDateTime::unix_timestamp),
        ),
        action_idempotency_key: format!(
            "action:lifecycle:{}:{template_key}:{}",
            snapshot.fan_id,
            snapshot
                .last_marketing_touch_at
                .map_or(0, OffsetDateTime::unix_timestamp)
        ),
    }))
}

fn release_candidate(
    snapshot: ReleasePlanSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::Release(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let ReleaseDecision::Request {
        milestone,
        confidence,
    } = evaluate_release(&snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let milestone_key = match milestone {
        ReleaseMilestone::SeedCalendar => "seed_calendar",
        ReleaseMilestone::Announcement => "announcement",
        ReleaseMilestone::StartPress => "start_press",
        ReleaseMilestone::FanWarmup => "fan_warmup",
        ReleaseMilestone::Countdown => "countdown",
        ReleaseMilestone::ReleaseDay => "release_day",
        ReleaseMilestone::Sustain => "sustain",
        ReleaseMilestone::Wrap => "wrap",
    };
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::ReleasePlan(snapshot.release_id),
        decision_kind: "execute_release_milestone",
        confidence,
        disposition,
        reason: "release timeline has a deterministic milestone due",
        input_snapshot: serde_json::to_value(&snapshot)?,
        policy_snapshot: policy_evidence(policy.clone(), domain_policy)?,
        action: AutopilotActionPayload::ExecuteReleaseMilestone {
            release_id: snapshot.release_id,
            title: snapshot.title.clone(),
            release_at: snapshot.release_at,
            milestone,
        },
        decision_key: format!(
            "decision:release:v{}:{}:{}:{}",
            policy.version,
            snapshot.release_id,
            milestone_key,
            snapshot.release_at.unix_timestamp()
        ),
        action_idempotency_key: format!("action:release:{}:{milestone_key}", snapshot.release_id),
    }))
}

fn live_opportunity_candidate(
    snapshot: LiveOpportunitySnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::LiveOpportunity(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let decision = evaluate_live_opportunity(snapshot, *domain_policy, now);
    let (score, confidence, forced_approval) = match decision {
        LiveOpportunityDecision::Hold => return Ok(None),
        LiveOpportunityDecision::PrepareForApproval { score, confidence } => {
            (score, confidence, true)
        }
        LiveOpportunityDecision::SubmitAutomatically { score, confidence } => {
            (score, confidence, false)
        }
    };
    let mut disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    if forced_approval && matches!(disposition, PolicyDisposition::AutoExecute) {
        disposition = PolicyDisposition::RequireApproval;
    }
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::TeamOpportunity(snapshot.opportunity_id),
        decision_kind: "apply_live_opportunity",
        confidence,
        disposition,
        reason: "verified live opportunity clears deterministic fit and economics gates",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy.clone(), domain_policy)?,
        action: AutopilotActionPayload::ApplyLiveOpportunity {
            opportunity_id: snapshot.opportunity_id,
            opportunity_kind: snapshot.kind,
            score,
        },
        decision_key: format!(
            "decision:live:v{}:{}:{score}:{}",
            policy.version,
            snapshot.opportunity_id,
            snapshot.deadline.map_or(0, OffsetDateTime::unix_timestamp)
        ),
        action_idempotency_key: format!("action:live:{}:apply", snapshot.opportunity_id),
    }))
}

fn funding_candidate(
    snapshot: FundingOpportunitySnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::Funding(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let decision = evaluate_funding(snapshot, *domain_policy, now);
    let (decision_kind, confidence, action, force_approval, key) = match decision {
        FundingDecision::Hold => return Ok(None),
        FundingDecision::PreparePackage { confidence } => (
            "prepare_funding_package",
            confidence,
            AutopilotActionPayload::PrepareFundingPackage {
                opportunity_id: snapshot.opportunity_id,
            },
            false,
            "prepare",
        ),
        FundingDecision::SubmitForApproval { confidence } => (
            "submit_funding_application",
            confidence,
            AutopilotActionPayload::SubmitFundingApplication {
                opportunity_id: snapshot.opportunity_id,
            },
            true,
            "submit",
        ),
    };
    let mut disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    if force_approval && matches!(disposition, PolicyDisposition::AutoExecute) {
        disposition = PolicyDisposition::RequireApproval;
    }
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::TeamOpportunity(snapshot.opportunity_id),
        decision_kind,
        confidence,
        disposition,
        reason: "eligible funding opportunity clears deterministic value and contribution gates",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy.clone(), domain_policy)?,
        action,
        decision_key: format!(
            "decision:funding:v{}:{}:{key}:{}",
            policy.version,
            snapshot.opportunity_id,
            snapshot.deadline.unix_timestamp()
        ),
        action_idempotency_key: format!("action:funding:{}:{key}", snapshot.opportunity_id),
    }))
}

fn merch_candidate(
    snapshot: MerchInventorySnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::Merchandising(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let MerchReorderDecision::RequestReorder {
        quantity,
        confidence,
    } = evaluate_reorder(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let subject = ActionSubject::MerchVariant(snapshot.variant_id);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject,
        decision_kind: "request_merch_reorder",
        confidence,
        disposition,
        reason: "projected stock coverage is below bounded target",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy.clone(), domain_policy)?,
        action: AutopilotActionPayload::RequestMerchReorder {
            variant_id: snapshot.variant_id,
            quantity,
        },
        decision_key: format!(
            "decision:merch:v{}:{}:{}:{}:{}",
            policy.version,
            snapshot.variant_id,
            snapshot.available_quantity,
            snapshot.sold_last_30d,
            snapshot
                .last_reorder_at
                .map_or(0, OffsetDateTime::unix_timestamp),
        ),
        action_idempotency_key: format!(
            "action:merch:{}:reorder:{}:{quantity}",
            snapshot.variant_id,
            snapshot.last_reorder_at.map_or_else(
                || "initial".to_owned(),
                |at| at.unix_timestamp().to_string()
            )
        ),
    }))
}

fn merch_price_candidate(
    snapshot: MerchPriceSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::MerchPricing(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let MerchPriceDecision::ChangePrice {
        direction,
        to_minor,
        confidence,
    } = evaluate_merch_price(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let Ok(from_minor) = i64::try_from(snapshot.current_price_minor) else {
        return Ok(None);
    };
    let Ok(to_minor) = i64::try_from(to_minor) else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let reason = match direction {
        MerchPriceDirection::Increase => {
            "recent demand acceleration and scarce stock justify one bounded price step"
        }
        MerchPriceDirection::Decrease => {
            "stagnant demand and excess stock justify one margin-safe price step"
        }
    };
    let subject = ActionSubject::MerchProduct(snapshot.product_id);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject,
        decision_kind: "change_merch_price",
        confidence,
        disposition,
        reason,
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy.clone(), domain_policy)?,
        action: AutopilotActionPayload::ChangeMerchPrice {
            product_id: snapshot.product_id,
            from_minor,
            to_minor,
            economics_version: snapshot.economics_version,
        },
        decision_key: format!(
            "decision:merch-price:v{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
            policy.version,
            snapshot.product_id,
            snapshot.current_price_minor,
            snapshot.minimum_price_minor,
            snapshot.maximum_price_minor,
            snapshot.economics_version,
            snapshot.available_quantity,
            snapshot.sold_last_7d,
            snapshot.sold_last_30d,
            snapshot
                .last_price_change_at
                .map_or(0, OffsetDateTime::unix_timestamp),
        ),
        action_idempotency_key: format!(
            "action:merch-price:{}:{}:{}:ev{}:{}",
            snapshot.product_id,
            from_minor,
            to_minor,
            snapshot.economics_version,
            snapshot
                .last_price_change_at
                .map_or(0, OffsetDateTime::unix_timestamp),
        ),
    }))
}

fn booking_candidate(
    snapshot: CityOpportunitySnapshot,
    targets: &[BookingTargetSnapshot],
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::BookingOpportunity(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let BookingOpportunityDecision::RequestOutreach { score, confidence } =
        evaluate_booking_opportunity(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let target_policy = BookingTargetSelectionPolicy::default();
    let expected_attendance = estimated_attendance(snapshot);
    let BookingTargetDecision::Selected {
        target_id,
        target_version,
        selection_score,
    } = select_booking_target(
        snapshot.city_id,
        expected_attendance,
        targets,
        target_policy,
        now,
    )
    else {
        return Ok(None);
    };
    let Some(target_snapshot) = targets.iter().find(|target| target.target_id == target_id) else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let subject = ActionSubject::City(snapshot.city_id);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject,
        decision_kind: "request_booking_outreach",
        confidence,
        disposition,
        reason: "city demand exceeds threshold and a verified booking target is eligible",
        input_snapshot: serde_json::json!({
            "city": snapshot,
            "target": target_snapshot,
            "selection_score": selection_score,
            "expected_attendance": expected_attendance,
        }),
        policy_snapshot: policy_evidence(
            policy.clone(),
            serde_json::json!({
                "opportunity": domain_policy,
                "target_selection": target_policy,
            }),
        )?,
        action: AutopilotActionPayload::RequestBookingOutreach {
            city_id: snapshot.city_id,
            target_id,
            target_version,
            target_name: target_snapshot.display_name.clone(),
            score,
            phase: BookingOutreachPhase::Initial,
        },
        decision_key: format!(
            "decision:booking:v{}:{}:{}:tv{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
            policy.version,
            snapshot.city_id,
            target_id,
            target_version,
            snapshot.active_fans,
            snapshot.new_fans_30d,
            snapshot.event_interests,
            snapshot.area_claims,
            snapshot
                .market_evidence
                .map_or(0, |value| value.score_basis_points),
            snapshot
                .market_evidence
                .map_or(0, |value| value.confidence.basis_points()),
            expected_attendance,
            selection_score,
            snapshot
                .last_outreach_at
                .map_or(0, OffsetDateTime::unix_timestamp),
        ),
        action_idempotency_key: format!(
            "action:booking:{}:{}:tv{}:{}",
            snapshot.city_id,
            target_id,
            target_version,
            snapshot.last_outreach_at.map_or_else(
                || "initial".to_owned(),
                |at| at.unix_timestamp().to_string(),
            )
        ),
    }))
}

fn booking_followup_candidate(
    target: &BookingTargetSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::BookingOpportunity(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let followup_policy = BookingFollowUpPolicy::default();
    let BookingFollowUpDecision::Request { confidence } =
        evaluate_booking_followup(target, followup_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::City(target.city_id),
        decision_kind: "request_booking_followup",
        confidence,
        disposition,
        reason: "verified booking target has not replied and the bounded follow-up window is due",
        input_snapshot: serde_json::to_value(target)?,
        policy_snapshot: policy_evidence(
            policy.clone(),
            serde_json::json!({"opportunity":domain_policy,"followup":followup_policy}),
        )?,
        action: AutopilotActionPayload::RequestBookingOutreach {
            city_id: target.city_id,
            target_id: target.target_id,
            target_version: target.version,
            target_name: target.display_name.clone(),
            score: 0,
            phase: BookingOutreachPhase::FollowUp,
        },
        decision_key: format!(
            "decision:booking-followup:v{}:{}:tv{}:{}:{}",
            policy.version,
            target.target_id,
            target.version,
            target.followup_count,
            target
                .last_outreach_at
                .map_or(0, OffsetDateTime::unix_timestamp)
        ),
        action_idempotency_key: format!(
            "action:booking-followup:{}:tv{}:{}",
            target.target_id,
            target.version,
            target.followup_count.saturating_add(1)
        ),
    }))
}

fn campaign_lifecycle_candidate(
    snapshot: EventCampaignSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::CampaignLifecycle(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let EventCampaignDecision::Request { phase, confidence } =
        evaluate_event_campaign(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::Event(snapshot.event_id),
        decision_kind: "request_event_campaign",
        confidence,
        disposition,
        reason: "event lifecycle phase is due for a consented first-party audience",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy.clone(), domain_policy)?,
        action: AutopilotActionPayload::RequestAudienceCampaign {
            event_id: snapshot.event_id,
            phase,
            template_key: phase.template_key().to_owned(),
        },
        decision_key: format!(
            "decision:event-campaign:v{}:{}:{:?}:{}:{}:{}",
            policy.version,
            snapshot.event_id,
            phase,
            snapshot.interested_fans,
            snapshot.paid_buyers,
            snapshot.attendees
        ),
        action_idempotency_key: format!("action:event-campaign:{}:{:?}", snapshot.event_id, phase),
    }))
}

fn merch_bundle_candidate(
    snapshot: MerchBundleSnapshot,
    policy: &AutopilotPolicy,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::MerchBundle(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let MerchBundleDecision::Recommend {
        bundle_price_minor,
        affinity_basis_points,
        confidence,
    } = evaluate_merch_bundle(snapshot, *domain_policy)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let (product_a, product_b) = if snapshot.product_a <= snapshot.product_b {
        (snapshot.product_a, snapshot.product_b)
    } else {
        (snapshot.product_b, snapshot.product_a)
    };
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::MerchProduct(product_a),
        decision_kind: "request_merch_bundle",
        confidence,
        disposition,
        reason: "repeat co-purchase evidence supports a bounded margin-safe bundle",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy.clone(), domain_policy)?,
        action: AutopilotActionPayload::RequestMerchBundle {
            product_a,
            product_b,
            bundle_price_minor,
            affinity_basis_points,
        },
        decision_key: format!(
            "decision:merch-bundle:v{}:{}:{}:{}:{}:{}",
            policy.version,
            product_a,
            product_b,
            snapshot.joint_orders,
            snapshot.orders_a,
            snapshot.orders_b
        ),
        action_idempotency_key: format!(
            "action:merch-bundle:{}:{}:{}",
            product_a, product_b, bundle_price_minor
        ),
    }))
}

fn outreach_candidate(
    snapshot: OutreachSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::Outreach(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let OutreachDecision::Request { phase, confidence } =
        evaluate_outreach(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let template_key = match snapshot.target_kind {
        crowdrelay_domain::outreach::OutreachTargetKind::Playlist => "outreach.playlist.v1",
        crowdrelay_domain::outreach::OutreachTargetKind::Radio => "outreach.radio.v1",
        crowdrelay_domain::outreach::OutreachTargetKind::Press => "outreach.press.v1",
        crowdrelay_domain::outreach::OutreachTargetKind::Creator => "outreach.creator.v1",
        crowdrelay_domain::outreach::OutreachTargetKind::SupportSlot => "outreach.support_slot.v1",
        crowdrelay_domain::outreach::OutreachTargetKind::Endorsement => "outreach.endorsement.v1",
        crowdrelay_domain::outreach::OutreachTargetKind::MediaPatronage => {
            "outreach.media_patronage.v1"
        }
    };
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::OutreachOpportunity(snapshot.opportunity_id),
        decision_kind: "request_relationship_outreach",
        confidence,
        disposition,
        reason: "verified relationship target matches a fresh high-relevance opportunity",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy.clone(), domain_policy)?,
        action: AutopilotActionPayload::RequestOutreach {
            opportunity_id: snapshot.opportunity_id,
            target_id: snapshot.target_id,
            target_version: snapshot.target_version,
            target_name: snapshot.target_id.to_string(),
            phase,
            template_key: template_key.to_owned(),
        },
        decision_key: format!(
            "decision:outreach:v{}:{}:{}:tv{}:{:?}:{}:{}",
            policy.version,
            snapshot.opportunity_id,
            snapshot.target_id,
            snapshot.target_version,
            phase,
            snapshot.relevance_basis_points,
            snapshot.observed_at.unix_timestamp()
        ),
        action_idempotency_key: format!(
            "action:outreach:{}:{}:{:?}:{}",
            snapshot.opportunity_id, snapshot.target_id, phase, snapshot.followup_count
        ),
    }))
}

fn content_candidate(
    snapshot: &ContentSupplySnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::ContentSupply(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let ContentSupplyDecision::Request {
        artifact,
        confidence,
    } = evaluate_content_supply(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::ContentSource(snapshot.source_id),
        decision_kind: "request_content_artifact",
        confidence,
        disposition,
        reason: "trusted source is missing one required deterministic content artifact",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy.clone(), domain_policy)?,
        action: AutopilotActionPayload::RequestContentArtifact {
            source_id: snapshot.source_id,
            source_version: snapshot.source_version,
            artifact,
            template_key: artifact.template_key().to_owned(),
        },
        decision_key: format!(
            "decision:content:v{}:{}:sv{}:{:?}",
            policy.version, snapshot.source_id, snapshot.source_version, artifact
        ),
        action_idempotency_key: format!(
            "action:content:{}:sv{}:{:?}",
            snapshot.source_id, snapshot.source_version, artifact
        ),
    }))
}

fn experiment_candidate(
    snapshot: &ExperimentSnapshot,
    policy: &AutopilotPolicy,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::Experimentation(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let decision = evaluate_experiment(snapshot, *domain_policy);
    let (winner, allocations, complete, confidence) = match decision {
        ExperimentDecision::Reallocate {
            winner,
            allocations,
            confidence,
        } => (winner, allocations, false, confidence),
        ExperimentDecision::Complete { winner, confidence } => {
            let allocations = snapshot
                .variants
                .iter()
                .map(|variant| {
                    (
                        variant.variant_id,
                        if variant.variant_id == winner {
                            10_000
                        } else {
                            0
                        },
                    )
                })
                .collect();
            (winner, allocations, true, confidence)
        }
        ExperimentDecision::Hold(_) => return Ok(None),
    };
    let typed_allocations = allocations
        .into_iter()
        .map(
            |(variant_id, allocation_basis_points)| ExperimentAllocation {
                variant_id,
                allocation_basis_points,
            },
        )
        .collect::<Vec<_>>();
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::Experiment(snapshot.experiment_id),
        decision_kind: if complete {
            "complete_experiment"
        } else {
            "reallocate_experiment"
        },
        confidence,
        disposition,
        reason: "aggregate experiment evidence shows a bounded material winner",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy.clone(), domain_policy)?,
        action: AutopilotActionPayload::AdjustExperiment {
            experiment_id: snapshot.experiment_id,
            expected_version: snapshot.version,
            winner_variant_id: winner,
            allocations: typed_allocations,
            complete,
        },
        decision_key: format!(
            "decision:experiment:v{}:{}:ev{}:{}:{}",
            policy.version,
            snapshot.experiment_id,
            snapshot.version,
            snapshot
                .variants
                .iter()
                .map(|v| format!(
                    "{}:{}:{}:{}",
                    v.variant_id, v.exposures, v.conversions, v.value_minor
                ))
                .collect::<Vec<_>>()
                .join("|"),
            complete
        ),
        action_idempotency_key: format!(
            "action:experiment:{}:ev{}:{}:{}",
            snapshot.experiment_id,
            snapshot.version,
            winner,
            snapshot.variants.iter().map(|v| v.exposures).sum::<u64>()
        ),
    }))
}

fn show_operations_candidate(
    snapshot: ShowTaskSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::ShowOperations(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let (action, decision_kind, reason, confidence) =
        match evaluate_show_task(snapshot, *domain_policy, now) {
            ShowOperationsDecision::AutoComplete { confidence } => (
                AutopilotActionPayload::CompleteShowTask {
                    event_id: snapshot.event_id,
                    task: snapshot.task,
                },
                "complete_verified_show_task",
                "first-party evidence proves a non-physical show task is complete",
                confidence,
            ),
            ShowOperationsDecision::EscalateHuman { confidence } => (
                AutopilotActionPayload::EscalateShowTask {
                    event_id: snapshot.event_id,
                    task: snapshot.task,
                },
                "escalate_show_task",
                "show task is due and requires human or physical confirmation",
                confidence,
            ),
            ShowOperationsDecision::Hold(_) => return Ok(None),
        };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject: ActionSubject::Event(snapshot.event_id),
        decision_kind,
        confidence,
        disposition,
        reason,
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy.clone(), domain_policy)?,
        action,
        decision_key: format!(
            "decision:show:v{}:{}:{:?}:{}:{}",
            policy.version,
            snapshot.event_id,
            snapshot.task,
            snapshot.verifiable_fact,
            snapshot
                .last_escalated_at
                .map_or(0, OffsetDateTime::unix_timestamp)
        ),
        action_idempotency_key: match evaluate_show_task(snapshot, *domain_policy, now) {
            ShowOperationsDecision::AutoComplete { .. } => format!(
                "action:show:{}:{:?}:complete",
                snapshot.event_id, snapshot.task
            ),
            _ => format!(
                "action:show:{}:{:?}:escalate:{}",
                snapshot.event_id,
                snapshot.task,
                snapshot
                    .last_escalated_at
                    .map_or(0, OffsetDateTime::unix_timestamp)
            ),
        },
    }))
}

fn promotion_candidate(
    snapshot: PromotionPerformanceSnapshot,
    policy: &AutopilotPolicy,
    now: OffsetDateTime,
) -> Result<Option<DecisionCandidate>, serde_json::Error> {
    let AutopilotPolicyConfig::PromotionBudget(domain_policy) = &policy.config else {
        return Ok(None);
    };
    let PromotionBudgetDecision::Adjust {
        from_minor,
        to_minor,
        roas_basis_points,
        confidence,
        ..
    } = evaluate_promotion_budget(snapshot, *domain_policy, now)
    else {
        return Ok(None);
    };
    let disposition = disposition(policy.autonomy_level, confidence, policy.minimum_confidence);
    let subject = ActionSubject::PromotionCampaign(snapshot.campaign_id);
    Ok(Some(DecisionCandidate {
        context: policy.context,
        subject,
        decision_kind: "adjust_promotion_budget",
        confidence,
        disposition,
        reason: "bounded promotion ROAS is outside configured performance band",
        input_snapshot: serde_json::to_value(snapshot)?,
        policy_snapshot: policy_evidence(policy.clone(), domain_policy)?,
        action: AutopilotActionPayload::RequestPromotionBudgetChange {
            campaign_id: snapshot.campaign_id,
            from_minor,
            to_minor,
            roas_basis_points,
        },
        decision_key: format!(
            "decision:promotion:v{}:{}:{}:{}:{}:{}",
            policy.version,
            snapshot.campaign_id,
            snapshot.current_daily_budget_minor,
            snapshot.spend_last_7d_minor,
            snapshot.attributed_revenue_last_7d_minor,
            snapshot.observed_at.unix_timestamp(),
        ),
        action_idempotency_key: format!(
            "action:promotion:{}:{}:{from_minor}:{to_minor}",
            snapshot.campaign_id,
            snapshot.last_budget_change_at.map_or_else(
                || "initial".to_owned(),
                |at| at.unix_timestamp().to_string(),
            )
        ),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crowdrelay_domain::{
        TicketTypeId,
        autonomy::{AutonomyLevel, Confidence, PolicyDisposition},
        pricing::TicketYieldPolicy,
    };

    #[test]
    fn recommend_policy_never_creates_auto_execute_disposition()
    -> Result<(), Box<dyn std::error::Error>> {
        let minimum = Confidence::from_basis_points(8_000)?;
        let policy = AutopilotPolicy {
            context: AutopilotContext::TicketYield,
            enabled: true,
            autonomy_level: AutonomyLevel::Recommend,
            minimum_confidence: minimum,
            max_actions_24h: 10,
            config: AutopilotPolicyConfig::TicketYield(TicketYieldPolicy::default()),
            version: 1,
            guarded_until: None,
            guardrail_reason: None,
        };
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let candidate = ticket_candidate(
            TicketYieldSnapshot {
                ticket_type_id: TicketTypeId::new(),
                current_price_minor: 3_000,
                paid_quantity: 80,
                capacity: 100,
                sale_capacity: 100,
                paid_last_72h: 8,
                days_to_event: 21,
                last_price_change_at: None,
                last_capacity_change_at: None,
                allocation_guardrail: None,
            },
            &policy,
            now,
        )?
        .ok_or_else(|| std::io::Error::other("candidate expected"))?;
        assert_eq!(candidate.disposition, PolicyDisposition::RecommendOnly);
        Ok(())
    }
}
