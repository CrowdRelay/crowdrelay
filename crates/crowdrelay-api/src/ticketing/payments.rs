pub async fn bind_stripe_checkout(
    State(state): State<crate::AppState>,
    Path(order_id): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<BindStripeCheckoutRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !bearer_sha256_matches_either(
        &headers,
        state.ticketing.commerce_api_key_sha256,
        state.ticketing.previous_commerce_api_key_sha256,
    ) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let order_id = match Uuid::parse_str(&order_id) {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    if payload.checkout_token.len() != 64
        || !payload
            .checkout_token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || !valid_stripe_id(&payload.stripe_checkout_session_id, "cs_")
    {
        return TicketingError::Invalid.response(request_id_value);
    }
    let request_id_text = trusted_request_id(&headers);
    let future = bind_stripe_checkout_inner(
        &state.ticketing,
        order_id,
        &payload,
        request_id_text.as_deref(),
    );
    match timeout(state.ticketing.operation_timeout, future).await {
        Ok(Ok(result)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(result),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => TicketingError::Unavailable.response(request_id_value),
    }
}

/// Cancels an unpaid order and releases its shared admission reservation.
pub async fn cancel_order(
    State(state): State<crate::AppState>,
    Path(order_id): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<CancelTicketOrderRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !bearer_sha256_matches_either(
        &headers,
        state.ticketing.commerce_api_key_sha256,
        state.ticketing.previous_commerce_api_key_sha256,
    ) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let order_id = match Uuid::parse_str(&order_id) {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    if payload.checkout_token.len() != 64
        || !payload
            .checkout_token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || clean_text(&payload.reason, 160).is_none()
    {
        return TicketingError::Invalid.response(request_id_value);
    }
    let request_id_text = trusted_request_id(&headers);
    let future = cancel_order_inner(
        &state.ticketing,
        order_id,
        &payload,
        request_id_text.as_deref(),
    );
    match timeout(state.ticketing.operation_timeout, future).await {
        Ok(Ok(order)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(order),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => TicketingError::Unavailable.response(request_id_value),
    }
}

/// Applies one verified Stripe event to a ticket order.
pub async fn stripe_event(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<StripeTicketEventRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    if !bearer_sha256_matches_either(
        &headers,
        state.ticketing.commerce_api_key_sha256,
        state.ticketing.previous_commerce_api_key_sha256,
    ) {
        return Problem::unauthorized(request_id_value)
            .private()
            .into_response();
    }
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return TicketingError::Invalid.response(request_id_value),
    };
    if !valid_stripe_event(&payload) {
        return TicketingError::Invalid.response(request_id_value);
    }
    let request_id_text = trusted_request_id(&headers);
    let future = stripe_event_inner(&state.ticketing, &payload, request_id_text.as_deref());
    match timeout(state.ticketing.operation_timeout.saturating_mul(3), future).await {
        Ok(Ok(result)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(result),
        )
            .into_response(),
        Ok(Err(error)) => error.response(request_id_value),
        Err(_) => TicketingError::Unavailable.response(request_id_value),
    }
}

async fn bind_stripe_checkout_inner(
    state: &TicketingState,
    order_id: Uuid,
    request: &BindStripeCheckoutRequest,
    request_id_value: Option<&str>,
) -> Result<StripeCheckoutBindingResponse, TicketingError> {
    let mut transaction = state.pool.begin().await.map_err(TicketingError::sqlx)?;
    let order = sqlx::query_as::<_, OrderRow>(ORDER_ROW_BY_ID_FOR_UPDATE)
        .bind(state.workspace_id.into_uuid())
        .bind(order_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(TicketingError::sqlx)?
        .ok_or(TicketingError::NotFound)?;

    let token_matches = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT checkout_token_hash = digest($3, 'sha256')
        FROM ticket_orders
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order_id)
    .bind(&request.checkout_token)
    .fetch_one(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if !token_matches {
        return Err(TicketingError::NotFound);
    }

    if let Some(existing_session) = &order.stripe_checkout_session_id {
        if existing_session != &request.stripe_checkout_session_id {
            return Err(TicketingError::Conflict);
        }
        transaction.commit().await.map_err(TicketingError::sqlx)?;
        return Ok(StripeCheckoutBindingResponse {
            order_id: order.id,
            public_reference: order.public_reference,
            stripe_checkout_session_id: existing_session.clone(),
            currency: order.currency,
            amount_gross_minor: order.amount_gross_minor,
            expires_at: order.expires_at,
        });
    }

    let now = OffsetDateTime::now_utc();
    if order.status != "reserved" || order.expires_at <= now {
        if order.status == "reserved" && order.expires_at <= now {
            release_order_reservation(&mut transaction, state.workspace_id, &order, "expired")
                .await?;
            transaction.commit().await.map_err(TicketingError::sqlx)?;
        }
        return Err(TicketingError::Conflict);
    }
    if request.stripe_expires_at < now || request.stripe_expires_at > order.expires_at {
        return Err(TicketingError::Invalid);
    }

    let updated_order = sqlx::query(
        r#"
        UPDATE ticket_orders
        SET status = 'checkout_created',
            stripe_checkout_session_id = $3,
            expires_at = $4
        WHERE workspace_id = $1 AND id = $2
          AND status = 'reserved'
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.id)
    .bind(&request.stripe_checkout_session_id)
    .bind(request.stripe_expires_at)
    .execute(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if updated_order.rows_affected() != 1 {
        return Err(TicketingError::Conflict);
    }

    append_audit(
        &mut transaction,
        state.workspace_id,
        "service",
        "ticket_order.checkout_bound",
        "ticket_order",
        order.id,
        request_id_value,
        json!({
            "stripe_checkout_session_id": request.stripe_checkout_session_id,
            "expires_at": request.stripe_expires_at,
        }),
    )
    .await?;

    transaction.commit().await.map_err(TicketingError::sqlx)?;
    Ok(StripeCheckoutBindingResponse {
        order_id: order.id,
        public_reference: order.public_reference,
        stripe_checkout_session_id: request.stripe_checkout_session_id.clone(),
        currency: order.currency,
        amount_gross_minor: order.amount_gross_minor,
        expires_at: request.stripe_expires_at,
    })
}

async fn cancel_order_inner(
    state: &TicketingState,
    order_id: Uuid,
    request: &CancelTicketOrderRequest,
    request_id_value: Option<&str>,
) -> Result<TicketOrderView, TicketingError> {
    let mut transaction = state.pool.begin().await.map_err(TicketingError::sqlx)?;
    let order = sqlx::query_as::<_, OrderRow>(ORDER_ROW_BY_ID_FOR_UPDATE)
        .bind(state.workspace_id.into_uuid())
        .bind(order_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(TicketingError::sqlx)?
        .ok_or(TicketingError::NotFound)?;
    let token_matches = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT checkout_token_hash = digest($3, 'sha256')
        FROM ticket_orders
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.id)
    .bind(&request.checkout_token)
    .fetch_one(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if !token_matches {
        return Err(TicketingError::NotFound);
    }
    if matches!(
        order.status.as_str(),
        "paid" | "partially_refunded" | "refunded"
    ) {
        return Err(TicketingError::Conflict);
    }
    if matches!(order.status.as_str(), "reserved" | "checkout_created") {
        release_order_reservation(&mut transaction, state.workspace_id, &order, "cancelled")
            .await?;
        append_audit(
            &mut transaction,
            state.workspace_id,
            "service",
            "ticket_order.cancelled",
            "ticket_order",
            order.id,
            request_id_value,
            json!({ "reason": request.reason.trim() }),
        )
        .await?;
    }
    let updated = load_order_row_by_id(&mut transaction, state.workspace_id, order.id).await?;
    let view = load_order_view_for_row(&mut transaction, state.workspace_id, updated).await?;
    transaction.commit().await.map_err(TicketingError::sqlx)?;
    Ok(view)
}

async fn stripe_event_inner(
    state: &TicketingState,
    request: &StripeTicketEventRequest,
    request_id_value: Option<&str>,
) -> Result<StripeTicketEventResponse, TicketingError> {
    let mut transaction = state.pool.begin().await.map_err(TicketingError::sqlx)?;
    let order = if let Some(session_id) = request.stripe_checkout_session_id.as_deref() {
        sqlx::query_as::<_, OrderRow>(ORDER_ROW_BY_STRIPE_SESSION_FOR_UPDATE)
            .bind(state.workspace_id.into_uuid())
            .bind(session_id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(TicketingError::sqlx)?
    } else if let Some(payment_intent_id) = request.stripe_payment_intent_id.as_deref() {
        sqlx::query_as::<_, OrderRow>(ORDER_ROW_BY_PAYMENT_INTENT_FOR_UPDATE)
            .bind(state.workspace_id.into_uuid())
            .bind(payment_intent_id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(TicketingError::sqlx)?
    } else {
        None
    }
    .ok_or(TicketingError::NotFound)?;

    let payload = serde_json::to_vec(request).map_err(|_| TicketingError::Invalid)?;
    let payload_hash: [u8; 32] = Sha256::digest(&payload).into();
    let existing_hash = sqlx::query_scalar::<_, Vec<u8>>(
        r#"
        SELECT payload_hash
        FROM ticket_stripe_events
        WHERE workspace_id = $1 AND stripe_event_id = $2
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(&request.stripe_event_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if let Some(existing_hash) = existing_hash {
        if existing_hash.as_slice() != payload_hash.as_slice() {
            return Err(TicketingError::Conflict);
        }
        let view = load_order_view_for_row(&mut transaction, state.workspace_id, order).await?;
        transaction.commit().await.map_err(TicketingError::sqlx)?;
        return Ok(StripeTicketEventResponse {
            received: true,
            duplicate: true,
            order: view,
        });
    }

    match request.event_type.as_str() {
        "checkout.session.completed" | "checkout.session.async_payment_succeeded" => {
            process_paid_order(&mut transaction, state, &order, request, request_id_value).await?;
        }
        "checkout.session.expired" | "checkout.session.async_payment_failed" => {
            release_unpaid_order(&mut transaction, state, &order, request, request_id_value)
                .await?;
        }
        "charge.refunded" | "refund.created" | "refund.updated" => {
            process_refund(&mut transaction, state, &order, request, request_id_value).await?;
        }
        _ => return Err(TicketingError::Invalid),
    }

    sqlx::query(
        r#"
        INSERT INTO ticket_stripe_events (
            workspace_id, ticket_order_id, stripe_event_id, event_type, payload_hash
        ) VALUES ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.id)
    .bind(&request.stripe_event_id)
    .bind(&request.event_type)
    .bind(payload_hash.as_slice())
    .execute(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;

    let updated = load_order_row_by_id(&mut transaction, state.workspace_id, order.id).await?;
    let view = load_order_view_for_row(&mut transaction, state.workspace_id, updated).await?;
    transaction.commit().await.map_err(TicketingError::sqlx)?;
    Ok(StripeTicketEventResponse {
        received: true,
        duplicate: false,
        order: view,
    })
}

async fn process_paid_order(
    transaction: &mut Transaction<'_, Postgres>,
    state: &TicketingState,
    order: &OrderRow,
    request: &StripeTicketEventRequest,
    request_id_value: Option<&str>,
) -> Result<(), TicketingError> {
    if matches!(
        order.status.as_str(),
        "paid" | "partially_refunded" | "refunded"
    ) {
        if order.stripe_payment_intent_id.as_deref() != request.stripe_payment_intent_id.as_deref()
        {
            return Err(TicketingError::Conflict);
        }
        return Ok(());
    }
    if order.status != "checkout_created" {
        return Err(TicketingError::Conflict);
    }
    let stripe_checkout_session_id = request
        .stripe_checkout_session_id
        .as_deref()
        .ok_or(TicketingError::Invalid)?;
    let payment_status = request.payment_status.as_deref().unwrap_or_default();
    if !matches!(payment_status, "paid" | "no_payment_required") {
        if request.event_type == "checkout.session.completed" {
            return Ok(());
        }
        return Err(TicketingError::Conflict);
    }
    let event_currency = request.currency.as_deref().map(str::to_ascii_uppercase);
    if request.amount_total_minor != Some(order.amount_gross_minor)
        || event_currency.as_deref() != Some(order.currency.as_str())
    {
        return Err(TicketingError::Conflict);
    }
    if order.amount_gross_minor > 0 && request.stripe_payment_intent_id.is_none() {
        return Err(TicketingError::Conflict);
    }
    if request
        .customer_email
        .as_deref()
        .is_some_and(|email| !email.eq_ignore_ascii_case(&order.buyer_email))
    {
        return Err(TicketingError::Conflict);
    }

    let pool = sqlx::query_as::<_, PoolCapacityRow>(
        r#"
        SELECT capacity, issued_count, reserved_count
        FROM admission_pools
        WHERE workspace_id = $1 AND id = $2
        FOR UPDATE
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.admission_pool_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    let items = load_order_items(transaction, state.workspace_id, order.id).await?;
    let ticket_count: i32 = items.iter().try_fold(0_i32, |total, item| {
        total
            .checked_add(item.quantity)
            .ok_or(TicketingError::Unexpected)
    })?;
    if pool.reserved_count < ticket_count || pool.issued_count + ticket_count > pool.capacity {
        return Err(TicketingError::Conflict);
    }

    let fan_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO fans (workspace_id, normalized_email, display_name, status)
        VALUES ($1, $2, $3, 'active')
        ON CONFLICT (workspace_id, normalized_email) DO UPDATE
        SET display_name = COALESCE(fans.display_name, EXCLUDED.display_name)
        RETURNING id
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(&order.buyer_email)
    .bind(&order.buyer_name)
    .fetch_one(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;

    let claim_expires_at = order
        .ends_at
        .unwrap_or_else(|| order.starts_at + time::Duration::hours(12))
        + time::Duration::days(1);
    let issued_rows = sqlx::query_as::<_, IssuedPaidTicketRow>(
        r#"
        WITH expanded AS (
            SELECT
                item.id AS order_item_id,
                ticket_type.slug AS ticket_type_slug,
                ticket_type.name AS ticket_type_name,
                generate_series(1, item.quantity) AS sequence
            FROM ticket_order_items AS item
            JOIN ticket_types AS ticket_type
              ON ticket_type.workspace_id = item.workspace_id
             AND ticket_type.id = item.ticket_type_id
            WHERE item.workspace_id = $1
              AND item.ticket_order_id = $2
        ), inserted AS (
            INSERT INTO admission_passes (
                id, workspace_id, event_id, admission_pool_id, fan_id,
                issuance_method, public_reference, claim_token_hash,
                claim_expires_at, claim_token_consumed_at, status,
                claimed_at, ticket_order_item_id, ticket_sequence,
                holder_name, holder_email
            )
            SELECT
                gen_random_uuid(), $1, $3, $4, $5, 'paid',
                'VIRYA-' || upper(encode(gen_random_bytes(16), 'hex')),
                NULL, $6, now(), 'claimed', now(), expanded.order_item_id,
                expanded.sequence, $7, $8
            FROM expanded
            ORDER BY expanded.order_item_id, expanded.sequence
            RETURNING
                id AS pass_id,
                ticket_order_item_id AS order_item_id,
                ticket_sequence AS sequence,
                public_reference
        )
        SELECT
            inserted.pass_id,
            inserted.order_item_id,
            expanded.ticket_type_slug,
            expanded.ticket_type_name,
            inserted.sequence,
            inserted.public_reference
        FROM inserted
        JOIN expanded
          ON expanded.order_item_id = inserted.order_item_id
         AND expanded.sequence = inserted.sequence
        ORDER BY inserted.order_item_id, inserted.sequence
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.id)
    .bind(order.event_id)
    .bind(order.admission_pool_id)
    .bind(fan_id)
    .bind(claim_expires_at)
    .bind(&order.buyer_name)
    .bind(&order.buyer_email)
    .fetch_all(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if issued_rows.len() != usize::try_from(ticket_count).map_err(|_| TicketingError::Unexpected)? {
        return Err(TicketingError::Unexpected);
    }
    let mut issued = Vec::with_capacity(issued_rows.len());
    for ticket in issued_rows {
        issued.push(json!({
            "pass_id": ticket.pass_id,
            "order_item_id": ticket.order_item_id,
            "ticket_type_slug": ticket.ticket_type_slug,
            "ticket_type_name": ticket.ticket_type_name,
            "sequence": ticket.sequence,
            "public_reference": ticket.public_reference,
        }));
    }

    let transferred = sqlx::query(
        r#"
        UPDATE admission_pools
        SET issued_count = issued_count + $3,
            reserved_count = reserved_count - $3
        WHERE workspace_id = $1 AND id = $2
          AND reserved_count >= $3
          AND issued_count + $3 <= capacity
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.admission_pool_id)
    .bind(ticket_count)
    .execute(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if transferred.rows_affected() != 1 {
        return Err(TicketingError::Conflict);
    }

    let paid_order = sqlx::query(
        r#"
        UPDATE ticket_orders
        SET status = 'paid',
            stripe_payment_intent_id = $3,
            paid_at = $4
        WHERE workspace_id = $1 AND id = $2
          AND status = 'checkout_created'
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.id)
    .bind(&request.stripe_payment_intent_id)
    .bind(request.occurred_at)
    .execute(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if paid_order.rows_affected() != 1 {
        return Err(TicketingError::Conflict);
    }

    insert_accounting_entry(
        transaction,
        state,
        order,
        request,
        "sale",
        order.amount_gross_minor,
        order.amount_net_minor,
        order.amount_vat_minor,
    )
    .await?;

    let token_key = state
        .checkout_token_key
        .ok_or(TicketingError::Unavailable)?;
    let checkout_token = derive_checkout_token(&token_key, order.id, &order.reservation_key)?;
    let qr_not_before = ticket_qr_not_before(order);
    let qr_expires_at = ticket_qr_expires_at(order)?;
    for ticket in &mut issued {
        let pass_id = ticket
            .get("pass_id")
            .and_then(Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or(TicketingError::Unexpected)?;
        let reference = ticket
            .get("public_reference")
            .and_then(Value::as_str)
            .ok_or(TicketingError::Unexpected)?;
        let qr_token = encode_ticket_qr(
            pass_id,
            order.event_id,
            reference,
            qr_not_before.unix_timestamp(),
            qr_expires_at.unix_timestamp(),
            &token_key,
        )
        .map_err(|_| TicketingError::Unexpected)?;
        let Some(object) = ticket.as_object_mut() else {
            return Err(TicketingError::Unexpected);
        };
        object.insert("qr_token".to_owned(), Value::String(qr_token));
        object.insert("qr_not_before".to_owned(), json!(qr_not_before));
        object.insert("qr_expires_at".to_owned(), json!(qr_expires_at));
    }

    append_outbox(
        transaction,
        state.workspace_id,
        "ticket.order.paid",
        request_id_value,
        json!({
            "order_id": order.id,
            "order_reference": order.public_reference,
            "event_id": order.event_id,
            "event_slug": order.event_slug,
            "event_title": order.event_title,
            "venue": order.venue,
            "timezone": order.timezone,
            "starts_at": order.starts_at,
            "buyer_email": order.buyer_email,
            "buyer_name": order.buyer_name,
            "buyer_locale": order.buyer_locale,
            "checkout_token": checkout_token,
            "invoice": invoice_payload(order),
            "currency": order.currency,
            "amount_gross_minor": order.amount_gross_minor,
            "amount_net_minor": order.amount_net_minor,
            "amount_vat_minor": order.amount_vat_minor,
            "vat_rate_basis_points": order.vat_rate_basis_points,
            "invoice_requested": order.invoice_requested,
            "stripe_checkout_session_id": stripe_checkout_session_id,
            "stripe_payment_intent_id": request.stripe_payment_intent_id,
            "tickets": issued,
        }),
    )
    .await?;
    append_audit(
        transaction,
        state.workspace_id,
        "service",
        "ticket_order.paid",
        "ticket_order",
        order.id,
        request_id_value,
        json!({
            "ticket_count": ticket_count,
            "amount_gross_minor": order.amount_gross_minor,
            "stripe_event_id": request.stripe_event_id,
        }),
    )
    .await?;
    Ok(())
}

async fn release_unpaid_order(
    transaction: &mut Transaction<'_, Postgres>,
    state: &TicketingState,
    order: &OrderRow,
    request: &StripeTicketEventRequest,
    request_id_value: Option<&str>,
) -> Result<(), TicketingError> {
    if matches!(
        order.status.as_str(),
        "paid" | "partially_refunded" | "refunded"
    ) {
        return Ok(());
    }
    if !matches!(order.status.as_str(), "reserved" | "checkout_created") {
        return Ok(());
    }
    let status = if request.event_type == "checkout.session.async_payment_failed" {
        "payment_failed"
    } else {
        "expired"
    };
    release_order_reservation(transaction, state.workspace_id, order, status).await?;
    append_audit(
        transaction,
        state.workspace_id,
        "service",
        "ticket_order.released",
        "ticket_order",
        order.id,
        request_id_value,
        json!({
            "status": status,
            "stripe_event_id": request.stripe_event_id,
        }),
    )
    .await?;
    Ok(())
}

async fn process_refund(
    transaction: &mut Transaction<'_, Postgres>,
    state: &TicketingState,
    order: &OrderRow,
    request: &StripeTicketEventRequest,
    request_id_value: Option<&str>,
) -> Result<(), TicketingError> {
    if !matches!(
        order.status.as_str(),
        "paid" | "partially_refunded" | "refunded"
    ) {
        return Err(TicketingError::Conflict);
    }
    let refunded = request
        .amount_refunded_minor
        .ok_or(TicketingError::Invalid)?;
    if refunded < order.amount_refunded_minor || refunded > order.amount_gross_minor {
        return Err(TicketingError::Conflict);
    }
    if refunded == order.amount_refunded_minor {
        return Ok(());
    }
    let full = refunded == order.amount_gross_minor;
    let revoked = if full {
        let changed = sqlx::query(
            r#"
            UPDATE admission_passes AS pass
            SET status = 'revoked'
            FROM ticket_order_items AS item
            WHERE item.workspace_id = pass.workspace_id
              AND item.id = pass.ticket_order_item_id
              AND item.workspace_id = $1
              AND item.ticket_order_id = $2
              AND pass.status IN ('issued', 'claimed')
            "#,
        )
        .bind(state.workspace_id.into_uuid())
        .bind(order.id)
        .execute(&mut **transaction)
        .await
        .map_err(TicketingError::sqlx)?
        .rows_affected();
        let revoked = i32::try_from(changed).map_err(|_| TicketingError::Unexpected)?;
        if revoked > 0 {
            let updated_pool = sqlx::query(
                r#"
                UPDATE admission_pools
                SET issued_count = issued_count - $3
                WHERE workspace_id = $1 AND id = $2
                  AND issued_count >= $3
                "#,
            )
            .bind(state.workspace_id.into_uuid())
            .bind(order.admission_pool_id)
            .bind(revoked)
            .execute(&mut **transaction)
            .await
            .map_err(TicketingError::sqlx)?;
            if updated_pool.rows_affected() != 1 {
                return Err(TicketingError::Unexpected);
            }
        }
        revoked
    } else {
        0
    };

    let updated_order = sqlx::query(
        r#"
        UPDATE ticket_orders
        SET status = $3,
            amount_refunded_minor = $4,
            refunded_at = CASE WHEN $5 THEN $6 ELSE refunded_at END
        WHERE workspace_id = $1 AND id = $2
          AND status IN ('paid', 'partially_refunded', 'refunded')
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.id)
    .bind(if full {
        "refunded"
    } else {
        "partially_refunded"
    })
    .bind(refunded)
    .bind(full)
    .bind(request.occurred_at)
    .execute(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if updated_order.rows_affected() != 1 {
        return Err(TicketingError::Conflict);
    }

    let refund_gross_minor = refunded
        .checked_sub(order.amount_refunded_minor)
        .ok_or(TicketingError::Unexpected)?;
    let previously_refunded_net_minor = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COALESCE(-sum(amount_net_minor), 0)::bigint
        FROM ticket_accounting_entries
        WHERE workspace_id = $1
          AND ticket_order_id = $2
          AND entry_kind = 'refund'
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    let target_refunded_net_minor = if full {
        order.amount_net_minor
    } else {
        proportional_minor(order.amount_net_minor, refunded, order.amount_gross_minor)?
    };
    let refund_net_minor = target_refunded_net_minor
        .checked_sub(previously_refunded_net_minor)
        .ok_or(TicketingError::Unexpected)?;
    let refund_vat_minor = refund_gross_minor
        .checked_sub(refund_net_minor)
        .ok_or(TicketingError::Unexpected)?;
    insert_accounting_entry(
        transaction,
        state,
        order,
        request,
        "refund",
        -refund_gross_minor,
        -refund_net_minor,
        -refund_vat_minor,
    )
    .await?;

    append_outbox(
        transaction,
        state.workspace_id,
        "ticket.order.refund_recorded",
        request_id_value,
        json!({
            "order_id": order.id,
            "order_reference": order.public_reference,
            "event_id": order.event_id,
            "buyer_email": order.buyer_email,
            "amount_gross_minor": order.amount_gross_minor,
            "amount_refunded_minor": refunded,
            "currency": order.currency,
            "full_refund": full,
            "revoked_ticket_count": revoked,
            "stripe_event_id": request.stripe_event_id,
        }),
    )
    .await?;
    append_audit(
        transaction,
        state.workspace_id,
        "service",
        "ticket_order.refund_recorded",
        "ticket_order",
        order.id,
        request_id_value,
        json!({
            "amount_refunded_minor": refunded,
            "full_refund": full,
            "revoked_ticket_count": revoked,
        }),
    )
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn insert_accounting_entry(
    transaction: &mut Transaction<'_, Postgres>,
    state: &TicketingState,
    order: &OrderRow,
    request: &StripeTicketEventRequest,
    entry_kind: &str,
    amount_gross_minor: i64,
    amount_net_minor: i64,
    amount_vat_minor: i64,
) -> Result<(), TicketingError> {
    let balance_values_are_consistent = match (request.stripe_fee_minor, request.stripe_net_minor) {
        (Some(fee), Some(net)) => amount_gross_minor
            .checked_sub(fee)
            .is_some_and(|expected| expected == net),
        (None, None) => true,
        _ => false,
    };
    if !balance_values_are_consistent {
        return Err(TicketingError::Conflict);
    }
    sqlx::query(
        r#"
        INSERT INTO ticket_accounting_entries (
            workspace_id, ticket_order_id, event_id, stripe_event_id,
            entry_kind, occurred_at, currency, vat_rate_basis_points,
            amount_gross_minor, amount_net_minor, amount_vat_minor,
            stripe_balance_transaction_id, stripe_fee_minor, stripe_net_minor,
            stripe_reporting_category
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8,
            $9, $10, $11, $12, $13, $14, $15
        )
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.id)
    .bind(order.event_id)
    .bind(&request.stripe_event_id)
    .bind(entry_kind)
    .bind(request.occurred_at)
    .bind(&order.currency)
    .bind(order.vat_rate_basis_points)
    .bind(amount_gross_minor)
    .bind(amount_net_minor)
    .bind(amount_vat_minor)
    .bind(&request.stripe_balance_transaction_id)
    .bind(request.stripe_fee_minor)
    .bind(request.stripe_net_minor)
    .bind(&request.stripe_reporting_category)
    .execute(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    Ok(())
}

fn proportional_minor(
    component_total: i64,
    cumulative_gross: i64,
    gross_total: i64,
) -> Result<i64, TicketingError> {
    if component_total < 0 || cumulative_gross < 0 || gross_total <= 0 {
        return Err(TicketingError::Unexpected);
    }
    let numerator = i128::from(component_total)
        .checked_mul(i128::from(cumulative_gross))
        .ok_or(TicketingError::Unexpected)?;
    let rounded = numerator
        .checked_add(i128::from(gross_total / 2))
        .ok_or(TicketingError::Unexpected)?
        / i128::from(gross_total);
    i64::try_from(rounded).map_err(|_| TicketingError::Unexpected)
}
