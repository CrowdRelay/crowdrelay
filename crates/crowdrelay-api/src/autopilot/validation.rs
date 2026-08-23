fn validate_city_market_signal(
    request: CityMarketSignalRequest,
) -> Result<UpsertCityMarketSignal, ()> {
    if !valid_market_source(&request.source)
        || request.score_basis_points > 10_000
        || request.confidence_basis_points > 10_000
        || request.expires_at <= request.observed_at
        || request.expires_at - request.observed_at > MARKET_SIGNAL_MAX_TTL
        || request.observed_at > OffsetDateTime::now_utc() + PROMOTION_STATE_CLOCK_SKEW
    {
        return Err(());
    }
    let confidence =
        Confidence::from_basis_points(request.confidence_basis_points).map_err(|_| ())?;
    Ok(UpsertCityMarketSignal {
        source: request.source,
        city_id: CityId::from_uuid(request.city_id),
        kind: request.signal_kind,
        score_basis_points: request.score_basis_points,
        confidence,
        observed_at: request.observed_at,
        expires_at: request.expires_at,
    })
}

fn validate_promotion_state(
    request: PromotionCampaignStateRequest,
) -> Result<UpsertPromotionCampaignState, ()> {
    if !valid_provider(&request.provider)
        || !valid_external_key(&request.external_campaign_key)
        || !valid_currency(&request.currency)
        || request.minimum_daily_budget_minor <= 0
        || request.current_daily_budget_minor < request.minimum_daily_budget_minor
        || request.maximum_daily_budget_minor < request.current_daily_budget_minor
        || request.spend_last_7d_minor < 0
        || request.spend_month_to_date_minor < 0
        || request.attributed_revenue_last_7d_minor < 0
        || request.expires_at <= request.observed_at
        || request.expires_at - request.observed_at > PROMOTION_STATE_MAX_TTL
        || request.observed_at > OffsetDateTime::now_utc() + PROMOTION_STATE_CLOCK_SKEW
        || request
            .last_budget_change_at
            .is_some_and(|value| value > request.observed_at)
    {
        return Err(());
    }
    Ok(UpsertPromotionCampaignState {
        provider: request.provider,
        external_campaign_key: request.external_campaign_key,
        event_id: request.event_id.map(EventId::from_uuid),
        currency: request.currency,
        current_daily_budget_minor: request.current_daily_budget_minor,
        minimum_daily_budget_minor: request.minimum_daily_budget_minor,
        maximum_daily_budget_minor: request.maximum_daily_budget_minor,
        spend_last_7d_minor: request.spend_last_7d_minor,
        spend_month_to_date_minor: request.spend_month_to_date_minor,
        attributed_revenue_last_7d_minor: request.attributed_revenue_last_7d_minor,
        active: request.active,
        last_budget_change_at: request.last_budget_change_at,
        observed_at: request.observed_at,
        expires_at: request.expires_at,
    })
}

fn valid_booking_name(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty() && trimmed.len() <= 200 && trimmed.chars().all(|ch| !ch.is_control())
}

fn valid_booking_email(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > 320 || trimmed.chars().any(char::is_whitespace) {
        return false;
    }
    let Some((local, domain)) = trimmed.rsplit_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

fn valid_market_source(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}

fn valid_provider(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn valid_external_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 160
        && value.chars().all(|character| !character.is_control())
}

fn valid_currency(value: &str) -> bool {
    value.len() == 3 && value.bytes().all(|byte| byte.is_ascii_uppercase())
}

fn parse_context(value: &str) -> Option<AutopilotContext> {
    match value {
        "ticket_yield" => Some(AutopilotContext::TicketYield),
        "fan_lifecycle" => Some(AutopilotContext::FanLifecycle),
        "campaign_lifecycle" => Some(AutopilotContext::CampaignLifecycle),
        "merchandising" => Some(AutopilotContext::Merchandising),
        "merch_pricing" => Some(AutopilotContext::MerchPricing),
        "merch_bundle" => Some(AutopilotContext::MerchBundle),
        "booking_opportunity" => Some(AutopilotContext::BookingOpportunity),
        "outreach" => Some(AutopilotContext::Outreach),
        "content_supply" => Some(AutopilotContext::ContentSupply),
        "promotion_budget" => Some(AutopilotContext::PromotionBudget),
        "experimentation" => Some(AutopilotContext::Experimentation),
        "show_operations" => Some(AutopilotContext::ShowOperations),
        "release" => Some(AutopilotContext::Release),
        "live_opportunity" => Some(AutopilotContext::LiveOpportunity),
        "funding" => Some(AutopilotContext::Funding),
        "beacon" => Some(AutopilotContext::Beacon),
        "show_growth" => Some(AutopilotContext::ShowGrowth),
        "growth_metrics" => Some(AutopilotContext::GrowthMetrics),
        "growth_debt" => Some(AutopilotContext::GrowthDebt),
        "outreach_supply" => Some(AutopilotContext::OutreachSupply),
        _ => None,
    }
}

#[allow(clippy::result_large_err)]
fn parse_idempotency_key(headers: &HeaderMap) -> Result<IdempotencyKey, Response> {
    let value = headers
        .get(&IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| IdempotencyKey::parse(value).ok());
    value.ok_or_else(|| {
        Problem::bad_request(request_id(headers))
            .private()
            .into_response()
    })
}

fn parsed_request_id(headers: &HeaderMap) -> Option<RequestId> {
    request_id(headers).and_then(|value| RequestId::parse(value).ok())
}

fn repository_problem(error: RepositoryError, request_id: Option<String>) -> Response {
    match error {
        RepositoryError::Unavailable => Problem::service_unavailable(request_id).private(),
        RepositoryError::NotFound => Problem::not_found(request_id).private(),
        RepositoryError::Conflict => Problem::conflict(request_id).private(),
        RepositoryError::Unexpected => Problem::internal(request_id).private(),
    }
    .into_response()
}

fn private_json<T: serde::Serialize>(status: StatusCode, body: T) -> Response {
    (status, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(body)).into_response()
}
