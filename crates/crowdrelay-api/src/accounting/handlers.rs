pub async fn get_profile(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    if !state.ticketing.admin_authorized(&headers) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    match timeout(state.ticketing.operation_timeout(), load_profile(&state)).await {
        Ok(Ok(profile)) => private_json(StatusCode::OK, profile),
        Ok(Err(AccountingError::NotFound)) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Ok(Err(_)) | Err(_) => Problem::service_unavailable(request_id_value)
            .private()
            .into_response(),
    }
}

pub async fn configure_profile(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ConfigureAccountingProfileRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !state.ticketing.admin_authorized(&headers) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let Some(profile) = normalize_profile(payload) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    match timeout(
        state.ticketing.operation_timeout(),
        upsert_profile(&state, profile),
    )
    .await
    {
        Ok(Ok(profile)) => private_json(StatusCode::OK, profile),
        Ok(Err(_)) | Err(_) => Problem::service_unavailable(request_id_value)
            .private()
            .into_response(),
    }
}

pub async fn preview_ticket_sales(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    query: Query<AccountingMonthQuery>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !state.ticketing.admin_authorized(&headers) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let Some(period) = AccountingPeriod::parse(&query.month, &query.currency) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    match timeout(
        state.ticketing.operation_timeout().saturating_mul(3),
        build_preview(&state, period),
    )
    .await
    {
        Ok(Ok(preview)) => private_json(StatusCode::OK, preview),
        Ok(Err(AccountingError::NotFound)) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Ok(Err(_)) | Err(_) => Problem::service_unavailable(request_id_value)
            .private()
            .into_response(),
    }
}

pub async fn finalize_ticket_sales(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<FinalizeAccountingDocumentRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !state.ticketing.admin_authorized(&headers) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let Some(period) = AccountingPeriod::parse(&payload.month, &payload.currency) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    let Some(document_number) = clean_text(&payload.document_number, MAX_DOCUMENT_NUMBER_CHARS)
    else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    match timeout(
        state.ticketing.operation_timeout().saturating_mul(4),
        finalize_document(&state, period, document_number),
    )
    .await
    {
        Ok(Ok(document)) => private_json(StatusCode::CREATED, document),
        Ok(Err(AccountingError::Conflict)) => Problem::conflict(request_id_value)
            .private()
            .into_response(),
        Ok(Err(AccountingError::NotFound)) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Ok(Err(_)) | Err(_) => Problem::service_unavailable(request_id_value)
            .private()
            .into_response(),
    }
}

pub async fn list_invoice_requests(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    query: Query<AccountingMonthQuery>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !state.ticketing.admin_authorized(&headers) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let Some(period) = AccountingPeriod::parse(&query.month, &query.currency) else {
        return Problem::bad_request(request_id_value)
            .private()
            .into_response();
    };
    match timeout(
        state.ticketing.operation_timeout().saturating_mul(2),
        load_invoice_requests(&state, period),
    )
    .await
    {
        Ok(Ok(items)) => private_json(StatusCode::OK, json!({ "items": items })),
        Ok(Err(_)) | Err(_) => Problem::service_unavailable(request_id_value)
            .private()
            .into_response(),
    }
}

pub async fn download_accounting_csv(
    State(state): State<crate::AppState>,
    Path(document_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    if !state.ticketing.admin_authorized(&headers) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let document_id = match Uuid::parse_str(&document_id) {
        Ok(value) => value,
        Err(_) => {
            return Problem::bad_request(request_id_value)
                .private()
                .into_response();
        }
    };
    let document = match timeout(
        state.ticketing.operation_timeout(),
        load_document(&state, document_id),
    )
    .await
    {
        Ok(Ok(value)) => value,
        Ok(Err(AccountingError::NotFound)) => {
            return Problem::not_found(request_id_value)
                .private()
                .into_response();
        }
        Ok(Err(_)) | Err(_) => {
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    let csv = snapshot_csv(&document.snapshot);
    let filename = format!(
        "{}-{}.csv",
        sanitize_filename(&document.document_number),
        document.currency
    );
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"ticket-sales.csv\""));
    (
        StatusCode::OK,
        [
            (CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE)),
            (
                CONTENT_TYPE,
                HeaderValue::from_static("text/csv; charset=utf-8"),
            ),
            (CONTENT_DISPOSITION, disposition),
        ],
        csv,
    )
        .into_response()
}
