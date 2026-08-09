pub async fn public_catalog(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    if !matches!(
        crate::ecosystem::feature_enabled(&state, "merch_inventory_enabled").await,
        Ok(true)
    ) || !matches!(inventory_ready(&state).await, Ok(true))
    {
        return CommerceError::Unavailable.response(request_id(&headers));
    }
    let request_id_value = request_id(&headers);
    match timeout(
        state.ticketing.operation_timeout(),
        load_catalog(&state, true),
    )
    .await
    {
        Ok(Ok(catalog)) => {
            let Some(etag) = merch_catalog_etag(&catalog) else {
                return (
                    StatusCode::OK,
                    [(CACHE_CONTROL, PUBLIC_CACHE)],
                    Json(catalog),
                )
                    .into_response();
            };
            let Ok(etag_header) = HeaderValue::from_str(&etag) else {
                return (
                    StatusCode::OK,
                    [(CACHE_CONTROL, PUBLIC_CACHE)],
                    Json(catalog),
                )
                    .into_response();
            };

            if merch_etag_matches(headers.get(IF_NONE_MATCH), &etag) {
                return (
                    StatusCode::NOT_MODIFIED,
                    [(CACHE_CONTROL, HeaderValue::from_static(PUBLIC_CACHE)), (ETAG, etag_header)],
                )
                    .into_response();
            }

            (
                StatusCode::OK,
                [(CACHE_CONTROL, HeaderValue::from_static(PUBLIC_CACHE)), (ETAG, etag_header)],
                Json(catalog),
            )
                .into_response()
        },
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn admin_catalog(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    match timeout(
        state.ticketing.operation_timeout(),
        load_catalog(&state, false),
    )
    .await
    {
        Ok(Ok(catalog)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(catalog),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn upsert_catalog(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<UpsertCatalogRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = upsert_catalog_inner(&state, payload);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(catalog)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(catalog),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn adjust_inventory(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<AdjustInventoryRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let mutation_key = match idempotency_key(&headers) {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = adjust_inventory_inner(&state, mutation_key, payload);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn inventory_activation(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    match timeout(
        state.ticketing.operation_timeout(),
        load_inventory_activation(&state),
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
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn inventory_overview(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    match timeout(
        state.ticketing.operation_timeout(),
        load_inventory_overview(&state),
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
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn inventory_stocktake(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<InventoryStocktakeRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let mutation_key = match idempotency_key(&headers) {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = inventory_stocktake_inner(&state, mutation_key, payload);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn mark_inventory_ready(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<MarkInventoryReadyRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = mark_inventory_ready_inner(&state, payload, request_id_value.as_deref());
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn reserve_inventory(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ReserveInventoryRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = reserve_inventory_inner(&state, payload);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(view)) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn commit_inventory(
    State(state): State<crate::AppState>,
    Path(reservation_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let reservation_id = match Uuid::parse_str(reservation_id.trim()) {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = commit_inventory_inner(&state, reservation_id);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn release_inventory(
    State(state): State<crate::AppState>,
    Path(reservation_id): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<ReleaseInventoryRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let reservation_id = match Uuid::parse_str(reservation_id.trim()) {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = release_inventory_inner(&state, reservation_id, payload.reason);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn promotion_recommendations(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let future = load_promotion_recommendations(&state);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(items)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(items),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn list_reward_campaigns(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let future = load_reward_campaigns(&state);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(items)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(items),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn create_reward_campaign(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateRewardCampaignRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = create_reward_campaign_inner(&state, payload);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(view)) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn cancel_reward_campaign(
    State(state): State<crate::AppState>,
    Path(draw_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let draw_id = match Uuid::parse_str(draw_id.trim()) {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = cancel_reward_campaign_inner(&state, draw_id);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn schedule_reward_campaign(
    State(state): State<crate::AppState>,
    Path(draw_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let draw_id = match Uuid::parse_str(draw_id.trim()) {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = schedule_reward_campaign_inner(&state, draw_id);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn list_reward_draws(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let future = load_reward_draws(&state);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(items)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(items),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn delete_reward_draw(
    State(state): State<crate::AppState>,
    Path(draw_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let draw_id = match Uuid::parse_str(draw_id.trim()) {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = delete_reward_draw_inner(&state, draw_id, request_id_value.as_deref());
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn list_reward_fulfillments(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let future = load_reward_fulfillments(&state);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(items)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(items),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}

pub async fn fulfill_reward(
    State(state): State<crate::AppState>,
    Path(winner_id): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<FulfillRewardRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let winner_id = match Uuid::parse_str(winner_id.trim()) {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return CommerceError::Invalid.response(request_id_value),
    };
    let future = fulfill_reward_inner(&state, winner_id, payload);
    match timeout(state.ticketing.operation_timeout(), future).await {
        Ok(Ok(view)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(view),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => CommerceError::Unavailable.response(request_id_value),
    }
}
