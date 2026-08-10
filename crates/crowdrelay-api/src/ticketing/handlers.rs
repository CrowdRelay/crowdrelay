/// Returns the currently configured public ticket offer for an event.
pub async fn public_sale(
    State(state): State<crate::AppState>,
    Path(event_slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !matches!(
        crate::ecosystem::feature_enabled(&state, "ticket_sales_enabled").await,
        Ok(true)
    ) {
        return Problem::service_unavailable(request_id(&headers))
            .private()
            .into_response();
    }
    let request_id_value = request_id(&headers);
    let event_slug = match EventSlug::parse(event_slug) {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let future = load_sale_view(&state.ticketing, event_slug.as_str(), false);
    match timeout(state.ticketing.operation_timeout, future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PUBLIC_REVALIDATE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => TicketingError::Unavailable.response(request_id_value),
    }
}

/// Creates or updates an event ticket sale and its price tiers.
pub async fn configure_sale(
    State(state): State<crate::AppState>,
    Path(event_slug): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<ConfigureTicketSaleRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !bearer_sha256_matches(&headers, state.ticketing.admin_api_key_sha256) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let event_slug = match EventSlug::parse(event_slug) {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let ticket_types = match normalize_ticket_types(&payload) {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    if !valid_sale_configuration(&payload) {
        return TicketingError::Invalid.response(request_id_value);
    }
    let request_id_text = trusted_request_id(&headers);
    let future = configure_sale_inner(
        &state.ticketing,
        event_slug.as_str(),
        &payload,
        &ticket_types,
        request_id_text.as_deref(),
    );
    match timeout(state.ticketing.operation_timeout.saturating_mul(2), future).await {
        Ok(Ok(())) => match timeout(
            state.ticketing.operation_timeout,
            load_sale_view(&state.ticketing, event_slug.as_str(), true),
        )
        .await
        {
            Ok(Ok(view)) => (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(view),
            )
                .into_response(),
            Ok(Err(error)) => error.response(request_id_value),
            Err(_) => TicketingError::Unavailable.response(request_id_value),
        },
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => TicketingError::Unavailable.response(request_id_value),
    }
}

/// Returns operational ticketing totals and recent orders for one event.
pub async fn admin_overview(
    State(state): State<crate::AppState>,
    Path(event_slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let event_slug = match EventSlug::parse(event_slug) {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let future = load_admin_overview(&state.ticketing, event_slug.as_str());
    match timeout(state.ticketing.operation_timeout.saturating_mul(2), future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => TicketingError::Unavailable.response(request_id_value),
    }
}

/// Atomically reserves ticket capacity before a Stripe Checkout Session exists.
pub async fn reserve_order(
    State(state): State<crate::AppState>,
    Path(event_slug): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<ReserveTicketOrderRequest>, JsonRejection>,
) -> Response {
    if !matches!(
        crate::ecosystem::feature_enabled(&state, "ticket_sales_enabled").await,
        Ok(true)
    ) {
        return Problem::service_unavailable(request_id(&headers))
            .private()
            .into_response();
    }
    let request_id_value = request_id(&headers);
    let Some(idempotency_key) = idempotency_key(&headers) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let event_slug = match EventSlug::parse(event_slug) {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let reservation = match normalize_reservation(event_slug.as_str(), payload) {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    let Some(checkout_token_key) = state.ticketing.checkout_token_key else {
        tracing::error!("ticket checkout token key is not configured");
        return TicketingError::Unavailable.response(request_id_value);
    };
    let request_id_text = trusted_request_id(&headers);
    let future = reserve_order_inner(
        &state.ticketing,
        event_slug.as_str(),
        &idempotency_key,
        request_id_text.as_deref(),
        &reservation,
        &checkout_token_key,
    );
    match timeout(state.ticketing.operation_timeout.saturating_mul(2), future).await {
        Ok(Ok(result)) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(result),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => TicketingError::Unavailable.response(request_id_value),
    }
}

/// Returns a private order view when the caller presents its checkout token.
pub async fn order_status(
    State(state): State<crate::AppState>,
    Path(order_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let order_id = match Uuid::parse_str(&order_id) {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let Some(token) = bearer_token(&headers) else {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    };
    let future = load_order_by_token(&state.ticketing, order_id, token);
    match timeout(state.ticketing.operation_timeout, future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => TicketingError::Unavailable.response(request_id_value),
    }
}

/// Returns the private ticket wallet, including durable QR credentials.
pub async fn order_wallet(
    State(state): State<crate::AppState>,
    Path(order_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let order_id = match Uuid::parse_str(&order_id) {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let Some(token) = bearer_token(&headers) else {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    };
    let Some(signing_key) = state.ticketing.checkout_token_key else {
        tracing::error!("ticket wallet signing key is not configured");
        return TicketingError::Unavailable.response(request_id_value);
    };
    let future = load_ticket_wallet(&state.ticketing, order_id, token, &signing_key);
    match timeout(state.ticketing.operation_timeout.saturating_mul(2), future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => TicketingError::Unavailable.response(request_id_value),
    }
}

/// Queues an idempotent re-delivery of the ticket wallet to the buyer e-mail.
pub async fn request_delivery(
    State(state): State<crate::AppState>,
    Path(order_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !matches!(
        crate::ecosystem::feature_enabled(&state, "ticket_delivery_enabled").await,
        Ok(true)
    ) {
        return Problem::service_unavailable(request_id(&headers))
            .private()
            .into_response();
    }
    let request_id_value = request_id(&headers);
    let order_id = match Uuid::parse_str(&order_id) {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let Some(token) = bearer_token(&headers) else {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    };
    let Some(idempotency_key) = idempotency_key(&headers) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let Some(signing_key) = state.ticketing.checkout_token_key else {
        tracing::error!("ticket wallet signing key is not configured");
        return TicketingError::Unavailable.response(request_id_value);
    };
    let request_id_text = trusted_request_id(&headers);
    let future = request_ticket_delivery(
        &state.ticketing,
        order_id,
        token,
        &idempotency_key,
        request_id_text.as_deref(),
        &signing_key,
    );
    match timeout(state.ticketing.operation_timeout.saturating_mul(3), future).await {
        Ok(Ok(view)) => (
            StatusCode::ACCEPTED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => TicketingError::Unavailable.response(request_id_value),
    }
}
