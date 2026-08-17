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
        assignee: match (
            row.assignee_member_id,
            row.assignee_member_key,
            row.assignee_display_name,
        ) {
            (Some(member_id), Some(member_key), Some(display_name)) => Some(TeamAssigneeSummary {
                member_id,
                member_key,
                display_name,
            }),
            _ => None,
        },
        assignment_due_at: row.assignment_due_at,
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
        manual_steps: manual_steps_from_metadata(row.executor_metadata.as_ref()),
    })
}

fn manual_steps_from_metadata(metadata: Option<&Value>) -> Vec<AutopilotManualStep> {
    let Some(items) = metadata
        .and_then(|value| value.get("manual_steps"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    items
        .iter()
        .take(8)
        .filter_map(|item| {
            let destination = item.get("destination")?.as_str()?.trim();
            let url = item.get("url")?.as_str()?.trim();
            let what_to_do = item.get("what_to_do")?.as_str()?.trim();
            let why_it_matters = item.get("why_it_matters")?.as_str()?.trim();
            if destination.is_empty()
                || url.is_empty()
                || what_to_do.is_empty()
                || why_it_matters.is_empty()
            {
                return None;
            }
            Some(AutopilotManualStep {
                destination: destination.chars().take(120).collect(),
                url: url.chars().take(500).collect(),
                what_to_do: what_to_do.chars().take(300).collect(),
                why_it_matters: why_it_matters.chars().take(300).collect(),
            })
        })
        .collect()
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
        "booking_reply_7d" => Ok(AutopilotMeasurementKind::BookingReply7d),
        "outreach_reply_7d" => Ok(AutopilotMeasurementKind::OutreachReply7d),
        "audience_ticket_revenue_72h" => Ok(AutopilotMeasurementKind::AudienceTicketRevenue72h),
        "show_ticket_revenue_7d" => Ok(AutopilotMeasurementKind::ShowTicketRevenue7d),
        "show_growth_surface_clicks_7d" => {
            Ok(AutopilotMeasurementKind::ShowGrowthSurfaceClicks7d)
        }
        "show_growth_attributed_ticket_orders_7d" => {
            Ok(AutopilotMeasurementKind::ShowGrowthAttributedTicketOrders7d)
        }
        "grassroots_activation_replies_14d" => {
            Ok(AutopilotMeasurementKind::GrassrootsActivationReplies14d)
        }
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
        "beacon" => Ok(AutopilotContext::Beacon),
        "show_growth" => Ok(AutopilotContext::ShowGrowth),
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
