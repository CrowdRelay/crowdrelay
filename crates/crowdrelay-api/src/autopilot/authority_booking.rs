pub async fn set_manager_booking_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ManagerBookingPolicyRequest>,
) -> Response {
    if request.expected_version < 0
        || !request.policy.is_valid()
        || request
            .source_revision
            .as_ref()
            .is_some_and(|value| value.len() > 200)
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .set_manager_booking_policy(
            state.ops.workspace_id(),
            SetManagerBookingPolicy {
                policy: request.policy,
                source: request.source,
                source_revision: request.source_revision,
                expected_version: request.expected_version,
            },
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn set_authority(
    State(state): State<AppState>,
    Path(context): Path<String>,
    headers: HeaderMap,
    Json(request): Json<AuthorityRequest>,
) -> Response {
    let context = match parse_context(&context) {
        Some(context) => context,
        None => {
            return Problem::not_found(request_id(&headers))
                .private()
                .into_response();
        }
    };
    if request.expected_version <= 0 || !(1..=1000).contains(&request.max_actions_24h) {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let minimum_confidence =
        match Confidence::from_basis_points(request.minimum_confidence_basis_points) {
            Ok(value) => value,
            Err(_) => {
                return Problem::bad_request(request_id(&headers))
                    .private()
                    .into_response();
            }
        };
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .set_authority(
            state.ops.workspace_id(),
            SetAutopilotAuthority {
                context,
                enabled: request.enabled,
                autonomy_level: request.autonomy_level,
                minimum_confidence,
                max_actions_24h: request.max_actions_24h,
                expected_version: request.expected_version,
            },
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn upsert_booking_target(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<BookingTargetRequest>,
) -> Response {
    if request.expected_version < 0
        || (request.expected_version > 0 && request.target_id.is_none())
        || request.priority > 100
        || request.relationship_score > 100
        || request
            .capacity
            .is_some_and(|capacity| capacity == 0 || capacity > 100_000)
        || !valid_booking_name(&request.display_name)
        || !valid_booking_email(&request.contact_email)
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    let command = UpsertBookingTarget {
        target_id: request.target_id.map(BookingTargetId::from_uuid),
        city_id: CityId::from_uuid(request.city_id),
        kind: request.target_kind,
        display_name: request.display_name,
        contact_email: request.contact_email,
        capacity: request.capacity,
        priority: request.priority,
        relationship_score: request.relationship_score,
        active: request.active,
        accepts_booking: request.accepts_booking,
        expected_version: request.expected_version,
    };
    match state
        .autopilot
        .upsert_booking_target(
            state.ops.workspace_id(),
            command,
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn upsert_ticket_allocation_guardrail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<TicketAllocationGuardrailRequest>,
) -> Response {
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    if request.expected_version < 0
        || request.minimum_capacity == 0
        || request.maximum_capacity < request.minimum_capacity
        || request.step_capacity == 0
        || request.step_capacity > request.maximum_capacity
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .upsert_ticket_allocation_guardrail(
            state.ops.workspace_id(),
            UpsertTicketAllocationGuardrail {
                ticket_type_id: TicketTypeId::from_uuid(request.ticket_type_id),
                minimum_capacity: request.minimum_capacity,
                maximum_capacity: request.maximum_capacity,
                step_capacity: request.step_capacity,
                expected_version: request.expected_version,
            },
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn upsert_merch_product_economics(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<MerchProductEconomicsRequest>,
) -> Response {
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    if request.expected_version < 0
        || request.minimum_price_minor < 0
        || request.maximum_price_minor < request.minimum_price_minor
        || request
            .unit_cost_minor
            .is_some_and(|cost| cost < 0 || cost > request.maximum_price_minor)
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .upsert_merch_product_economics(
            state.ops.workspace_id(),
            UpsertMerchProductEconomics {
                product_id: MerchProductId::from_uuid(request.product_id),
                minimum_price_minor: request.minimum_price_minor,
                maximum_price_minor: request.maximum_price_minor,
                unit_cost_minor: request.unit_cost_minor,
                expected_version: request.expected_version,
            },
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

pub async fn upsert_promotion_budget_guardrail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<PromotionBudgetGuardrailRequest>,
) -> Response {
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !valid_currency(&request.currency)
        || request.maximum_total_daily_budget_minor <= 0
        || request.maximum_monthly_spend_minor <= 0
        || request.expected_version < 0
    {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .upsert_promotion_budget_guardrail(
            state.ops.workspace_id(),
            UpsertPromotionBudgetGuardrail {
                currency: request.currency,
                maximum_total_daily_budget_minor: request.maximum_total_daily_budget_minor,
                maximum_monthly_spend_minor: request.maximum_monthly_spend_minor,
                expected_version: request.expected_version,
            },
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

/// Reads the band's vehicles and rates, with the version to edit against.
pub async fn tour_economics(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match state
        .autopilot
        .load_tour_economics_config(state.ops.workspace_id())
        .await
    {
        Ok(config) => private_json(StatusCode::OK, config),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

/// Sets them. Every gig the agent costs uses these numbers, so a wrong one here
/// is a wrong answer on every booking decision until it is fixed.
pub async fn set_tour_economics(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<TourEconomicsRequest>,
) -> Response {
    if request.expected_version < 0 || !request.policy.is_valid() {
        return Problem::bad_request(request_id(&headers))
            .private()
            .into_response();
    }
    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let request_id_value = parsed_request_id(&headers);
    match state
        .autopilot
        .set_tour_economics(
            state.ops.workspace_id(),
            SetTourEconomics {
                policy: request.policy,
                expected_version: request.expected_version,
            },
            &idempotency_key,
            request_id_value.as_ref(),
        )
        .await
    {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

/// Which channels produced people who stayed.
///
/// Signups and activated fans side by side, and the unattributable part kept in
/// view rather than dropped — a report that hides its unknowns is how a large
/// attribution gap goes unnoticed for a month.
pub async fn acquisition_channels(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match state
        .autopilot
        .load_acquisition_channels(state.ops.workspace_id(), OffsetDateTime::now_utc())
        .await
    {
        Ok(channels) => private_json(StatusCode::OK, channels),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}
