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
        "beacon" => AutopilotContext::Beacon,
        "show_growth" => AutopilotContext::ShowGrowth,
        "growth_metrics" => AutopilotContext::GrowthMetrics,
        "growth_debt" => AutopilotContext::GrowthDebt,
        "outreach_supply" => AutopilotContext::OutreachSupply,
        "plays" => AutopilotContext::Plays,
        _ => return Err(RepositoryError::Unexpected),
    };
    let autonomy_level = parse_autonomy_level(&row.autonomy_level)?;
    let confidence = u16::try_from(row.minimum_confidence_basis_points)
        .ok()
        .and_then(|value| Confidence::from_basis_points(value).ok())
        .ok_or(RepositoryError::Unexpected)?;
    let config = AutopilotPolicyConfig::parse_for(context, row.config)
        .map_err(|_| RepositoryError::Unexpected)?;
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
        paid_ticket_count: bounded_u32(row.paid_ticket_count)?,
        qualified_referrals: bounded_u32(row.qualified_referrals)?,
        last_qualified_referral_at: row.last_qualified_referral_at,
        has_referral_code: row.has_referral_code,
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
