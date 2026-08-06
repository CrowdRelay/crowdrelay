fn valid_sale_configuration(request: &ConfigureTicketSaleRequest) -> bool {
    let currency = request.currency.trim();
    currency.len() == 3
        && currency.bytes().all(|byte| byte.is_ascii_alphabetic())
        && (0..=10_000).contains(&request.vat_rate_basis_points)
        && (1..=1_000_000).contains(&request.capacity)
        && (1..=100).contains(&request.max_per_order)
        && (2_100..=86_400).contains(&request.hold_seconds)
        && request.sales_open_at < request.sales_close_at
        && !request.ticket_types.is_empty()
        && request.ticket_types.len() <= MAX_TICKET_TYPES
}

fn normalize_ticket_types(
    request: &ConfigureTicketSaleRequest,
) -> Result<Vec<NormalizedTicketType>, TicketingError> {
    let mut slugs = HashSet::with_capacity(request.ticket_types.len());
    let mut normalized = Vec::with_capacity(request.ticket_types.len());
    for ticket_type in &request.ticket_types {
        let slug = EventSlug::parse(ticket_type.slug.trim())
            .map_err(|_| TicketingError::Invalid)?
            .as_str()
            .to_owned();
        if !slugs.insert(slug.clone()) {
            return Err(TicketingError::Invalid);
        }
        let name = clean_text(&ticket_type.name, MAX_NAME_CHARS).ok_or(TicketingError::Invalid)?;
        let description = match ticket_type.description.as_deref() {
            Some(value) => {
                Some(clean_text(value, MAX_DESCRIPTION_CHARS).ok_or(TicketingError::Invalid)?)
            }
            None => None,
        };
        if !(1..=1_000_000_000).contains(&ticket_type.price_gross_minor)
            || ticket_type
                .capacity
                .is_some_and(|capacity| !(1..=request.capacity).contains(&capacity))
            || !(-100_000..=100_000).contains(&ticket_type.sort_order)
        {
            return Err(TicketingError::Invalid);
        }
        normalized.push(NormalizedTicketType {
            slug,
            name,
            description,
            price_gross_minor: ticket_type.price_gross_minor,
            capacity: ticket_type.capacity,
            sort_order: ticket_type.sort_order,
            active: ticket_type.active,
        });
    }
    Ok(normalized)
}

fn normalize_reservation(
    event_slug: &str,
    request: ReserveTicketOrderRequest,
) -> Result<NormalizedReservation, TicketingError> {
    if request.items.is_empty() || request.items.len() > MAX_ORDER_LINES {
        return Err(TicketingError::Invalid);
    }
    let buyer_email =
        NormalizedEmail::parse(request.buyer_email).map_err(|_| TicketingError::Invalid)?;
    let buyer_name = match request.buyer_name.as_deref() {
        Some(value) => Some(clean_text(value, MAX_NAME_CHARS).ok_or(TicketingError::Invalid)?),
        None => None,
    };
    let buyer_locale = match request.buyer_locale.trim().to_ascii_lowercase().as_str() {
        "pl" => "pl".to_owned(),
        "en" => "en".to_owned(),
        _ => return Err(TicketingError::Invalid),
    };
    let invoice_details =
        normalize_invoice_details(request.invoice_requested, request.invoice_details)?;
    let mut quantities = BTreeMap::<String, i32>::new();
    for item in request.items {
        if !(1..=100).contains(&item.quantity) {
            return Err(TicketingError::Invalid);
        }
        let slug = EventSlug::parse(item.ticket_type_slug.trim())
            .map_err(|_| TicketingError::Invalid)?
            .as_str()
            .to_owned();
        let entry = quantities.entry(slug).or_insert(0);
        *entry = entry
            .checked_add(item.quantity)
            .ok_or(TicketingError::Invalid)?;
    }
    let total_quantity = quantities.values().try_fold(0_i32, |total, quantity| {
        total.checked_add(*quantity).ok_or(TicketingError::Invalid)
    })?;
    let items: Vec<(String, i32)> = quantities.into_iter().collect();
    let request_hash: [u8; 32] = Sha256::digest(
        serde_json::to_vec(&json!({
            "event_slug": event_slug,
            "buyer_email": buyer_email.as_str(),
            "buyer_name": buyer_name.as_deref(),
            "buyer_locale": &buyer_locale,
            "invoice_requested": request.invoice_requested,
            "invoice_details": &invoice_details,
            "items": &items,
        }))
        .map_err(|_| TicketingError::Invalid)?,
    )
    .into();
    Ok(NormalizedReservation {
        buyer_email,
        buyer_name,
        buyer_locale,
        invoice_requested: request.invoice_requested,
        invoice_details,
        items,
        total_quantity,
        request_hash,
    })
}

fn normalize_invoice_details(
    invoice_requested: bool,
    details: Option<InvoiceDetailsRequest>,
) -> Result<Option<InvoiceDetailsRequest>, TicketingError> {
    if !invoice_requested {
        return details
            .is_none()
            .then_some(None)
            .ok_or(TicketingError::Invalid);
    }
    let details = details.ok_or(TicketingError::Invalid)?;
    let buyer_type = match details.buyer_type.trim().to_ascii_lowercase().as_str() {
        "individual" => "individual".to_owned(),
        "company" => "company".to_owned(),
        _ => return Err(TicketingError::Invalid),
    };
    let address_line1 = clean_text(&details.address_line1, MAX_INVOICE_TEXT_CHARS)
        .ok_or(TicketingError::Invalid)?;
    let postal_code = clean_text(&details.postal_code, 32).ok_or(TicketingError::Invalid)?;
    let city = clean_text(&details.city, 120).ok_or(TicketingError::Invalid)?;
    let country_code = details.country_code.trim().to_ascii_uppercase();
    if country_code.len() != 2 || !country_code.bytes().all(|byte| byte.is_ascii_uppercase()) {
        return Err(TicketingError::Invalid);
    }
    let (company_name, tax_id, full_name) = if buyer_type == "company" {
        (
            Some(
                clean_text(details.company_name.as_deref().unwrap_or_default(), 200)
                    .ok_or(TicketingError::Invalid)?,
            ),
            Some(
                clean_text(details.tax_id.as_deref().unwrap_or_default(), 32)
                    .ok_or(TicketingError::Invalid)?,
            ),
            None,
        )
    } else {
        (
            None,
            None,
            Some(
                clean_text(details.full_name.as_deref().unwrap_or_default(), 200)
                    .ok_or(TicketingError::Invalid)?,
            ),
        )
    };
    Ok(Some(InvoiceDetailsRequest {
        buyer_type,
        company_name,
        tax_id,
        full_name,
        address_line1,
        postal_code,
        city,
        country_code,
    }))
}

fn split_gross(gross_minor: i64, vat_rate_basis_points: i32) -> Result<(i64, i64), TicketingError> {
    if gross_minor < 0 || !(0..=10_000).contains(&vat_rate_basis_points) {
        return Err(TicketingError::Invalid);
    }
    let divisor = i128::from(10_000 + vat_rate_basis_points);
    let numerator = i128::from(gross_minor)
        .checked_mul(10_000)
        .ok_or(TicketingError::Invalid)?;
    let net = numerator
        .checked_add(divisor / 2)
        .ok_or(TicketingError::Invalid)?
        / divisor;
    let net = i64::try_from(net).map_err(|_| TicketingError::Invalid)?;
    let vat = gross_minor
        .checked_sub(net)
        .ok_or(TicketingError::Invalid)?;
    Ok((net, vat))
}

fn derive_checkout_token(
    key: &[u8; 32],
    order_id: Uuid,
    reservation_key: &str,
) -> Result<String, TicketingError> {
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| TicketingError::Unexpected)?;
    mac.update(CHECKOUT_TOKEN_CONTEXT);
    mac.update(order_id.as_bytes());
    mac.update(reservation_key.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn order_public_reference(order_id: Uuid) -> String {
    let suffix: String = order_id
        .simple()
        .to_string()
        .chars()
        .skip(16)
        .collect::<String>()
        .to_ascii_uppercase();
    format!("VRY-ORD-{suffix}")
}

fn valid_stripe_event(request: &StripeTicketEventRequest) -> bool {
    let checkout_event = matches!(
        request.event_type.as_str(),
        "checkout.session.completed"
            | "checkout.session.async_payment_succeeded"
            | "checkout.session.expired"
            | "checkout.session.async_payment_failed"
    );
    let refund_event = matches!(
        request.event_type.as_str(),
        "charge.refunded" | "refund.created" | "refund.updated"
    );
    (checkout_event || refund_event)
        && valid_stripe_id(&request.stripe_event_id, "evt_")
        && request
            .stripe_checkout_session_id
            .as_deref()
            .is_none_or(|value| valid_stripe_id(value, "cs_"))
        && request
            .stripe_payment_intent_id
            .as_deref()
            .is_none_or(|value| valid_stripe_id(value, "pi_"))
        && (!checkout_event || request.stripe_checkout_session_id.is_some())
        && (!refund_event || request.stripe_payment_intent_id.is_some())
        && request.amount_total_minor.is_none_or(|value| value >= 0)
        && request.amount_refunded_minor.is_none_or(|value| value >= 0)
        && request.currency.as_deref().is_none_or(|value| {
            value.len() == 3 && value.bytes().all(|byte| byte.is_ascii_alphabetic())
        })
        && request
            .stripe_balance_transaction_id
            .as_deref()
            .is_none_or(|value| valid_stripe_id(value, "txn_"))
        && (request.stripe_fee_minor.is_some() == request.stripe_net_minor.is_some())
        && (request.stripe_balance_transaction_id.is_some() == request.stripe_fee_minor.is_some())
        && request
            .stripe_reporting_category
            .as_deref()
            .is_none_or(|value| {
                !value.trim().is_empty()
                    && value.len() <= 80
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            })
}

fn valid_checkout_token(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_stripe_id(value: &str, prefix: &str) -> bool {
    value.len() <= 255
        && value.starts_with(prefix)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn clean_text(value: &str, max_chars: usize) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.chars().count() <= max_chars).then(|| value.to_owned())
}

fn mask_email(email: &str) -> String {
    let Some((local, domain)) = email.split_once('@') else {
        return "***".to_owned();
    };
    let first = local.chars().next().unwrap_or('*');
    format!("{first}***@{domain}")
}

fn idempotency_key(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(&IDEMPOTENCY_KEY)?.to_str().ok()?.trim();
    if value.is_empty() || value.len() > 128 || !value.is_ascii() {
        return None;
    }
    Some(value.to_owned())
}

fn trusted_request_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get(&X_REQUEST_ID)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn ticket_qr_not_before(order: &OrderRow) -> OffsetDateTime {
    order
        .doors_at
        .unwrap_or(order.starts_at)
        .saturating_sub(time::Duration::hours(6))
}

fn ticket_qr_expires_at(order: &OrderRow) -> Result<OffsetDateTime, TicketingError> {
    order
        .ends_at
        .unwrap_or_else(|| order.starts_at + time::Duration::hours(12))
        .checked_add(time::Duration::hours(24))
        .ok_or(TicketingError::Unexpected)
}

fn invoice_payload(order: &OrderRow) -> Value {
    if !order.invoice_requested {
        return Value::Null;
    }
    json!({
        "buyer_type": order.invoice_buyer_type,
        "company_name": order.invoice_company_name,
        "tax_id": order.invoice_tax_id,
        "full_name": order.invoice_full_name,
        "address_line1": order.invoice_address_line1,
        "postal_code": order.invoice_postal_code,
        "city": order.invoice_city,
        "country_code": order.invoice_country_code,
    })
}

async fn load_ticket_wallet(
    state: &TicketingState,
    order_id: Uuid,
    token: &str,
    signing_key: &[u8; 32],
) -> Result<TicketWalletView, TicketingError> {
    let order = load_order_row_by_token_pool(state, order_id, token).await?;
    build_ticket_wallet_pool(state, order, signing_key).await
}

async fn load_order_row_by_token_pool(
    state: &TicketingState,
    order_id: Uuid,
    token: &str,
) -> Result<OrderRow, TicketingError> {
    if !valid_checkout_token(token) {
        return Err(TicketingError::NotFound);
    }
    let query = format!(
        "{ORDER_ROW_BASE} WHERE orders.workspace_id = $1 AND orders.id = $2 AND orders.checkout_token_hash = digest($3, 'sha256')"
    );
    sqlx::query_as::<_, OrderRow>(&query)
        .bind(state.workspace_id.into_uuid())
        .bind(order_id)
        .bind(token)
        .fetch_optional(&state.pool)
        .await
        .map_err(TicketingError::sqlx)?
        .ok_or(TicketingError::NotFound)
}

async fn build_ticket_wallet_pool(
    state: &TicketingState,
    order: OrderRow,
    signing_key: &[u8; 32],
) -> Result<TicketWalletView, TicketingError> {
    if !matches!(
        order.status.as_str(),
        "paid" | "partially_refunded" | "refunded"
    ) {
        return Err(TicketingError::Conflict);
    }
    let order_view =
        load_order_view_pool(&state.pool, state.workspace_id, clone_order_row(&order)).await?;
    let rows = sqlx::query_as::<_, TicketWalletRow>(
        r#"
        SELECT
            pass.id AS pass_id,
            item.id AS order_item_id,
            ticket_type.slug AS ticket_type_slug,
            ticket_type.name AS ticket_type_name,
            pass.ticket_sequence AS sequence,
            pass.public_reference,
            pass.status,
            pass.holder_name,
            pass.holder_email,
            pass.redeemed_at
        FROM admission_passes AS pass
        JOIN ticket_order_items AS item
          ON item.workspace_id = pass.workspace_id
         AND item.id = pass.ticket_order_item_id
        JOIN ticket_types AS ticket_type
          ON ticket_type.workspace_id = item.workspace_id
         AND ticket_type.id = item.ticket_type_id
        WHERE item.workspace_id = $1 AND item.ticket_order_id = $2
        ORDER BY ticket_type.sort_order, item.id, pass.ticket_sequence
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order.id)
    .fetch_all(&state.pool)
    .await
    .map_err(TicketingError::sqlx)?;
    let qr_not_before = ticket_qr_not_before(&order);
    let qr_expires_at = ticket_qr_expires_at(&order)?;
    let tickets = rows
        .into_iter()
        .map(|row| {
            let qr_token = if row.status == "claimed" {
                Some(
                    encode_ticket_qr(
                        row.pass_id,
                        order.event_id,
                        &row.public_reference,
                        qr_not_before.unix_timestamp(),
                        qr_expires_at.unix_timestamp(),
                        signing_key,
                    )
                    .map_err(|_| TicketingError::Unexpected)?,
                )
            } else {
                None
            };
            Ok(TicketWalletPassView {
                pass_id: row.pass_id,
                order_item_id: row.order_item_id,
                ticket_type_slug: row.ticket_type_slug,
                ticket_type_name: row.ticket_type_name,
                sequence: row.sequence,
                public_reference: row.public_reference,
                status: row.status,
                holder_name: row.holder_name,
                holder_email_masked: mask_email(&row.holder_email),
                redeemed_at: row.redeemed_at,
                qr_token,
                qr_not_before,
                qr_expires_at,
            })
        })
        .collect::<Result<Vec<_>, TicketingError>>()?;
    Ok(TicketWalletView {
        order: order_view,
        tickets,
    })
}

async fn request_ticket_delivery(
    state: &TicketingState,
    order_id: Uuid,
    token: &str,
    idempotency_key: &str,
    request_id_value: Option<&str>,
    signing_key: &[u8; 32],
) -> Result<TicketDeliveryRequestResponse, TicketingError> {
    let mut transaction = state.pool.begin().await.map_err(TicketingError::sqlx)?;
    configure_transaction(&mut transaction, state).await?;
    if !valid_checkout_token(token) {
        return Err(TicketingError::NotFound);
    }
    let order = sqlx::query_as::<_, OrderRow>(ORDER_ROW_BY_ID_FOR_UPDATE)
        .bind(state.workspace_id.into_uuid())
        .bind(order_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(TicketingError::sqlx)?
        .ok_or(TicketingError::NotFound)?;
    let token_matches = sqlx::query_scalar::<_, bool>(
        "SELECT checkout_token_hash = digest($3, 'sha256') FROM ticket_orders WHERE workspace_id = $1 AND id = $2",
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order_id)
    .bind(token)
    .fetch_one(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if !token_matches {
        return Err(TicketingError::NotFound);
    }
    if !matches!(order.status.as_str(), "paid" | "partially_refunded") {
        return Err(TicketingError::Conflict);
    }
    let request_hash: [u8; 32] =
        Sha256::digest(format!("{order_id}:{idempotency_key}").as_bytes()).into();
    let existing = sqlx::query_as::<_, (Vec<u8>, OffsetDateTime)>(
        "SELECT request_hash, created_at FROM ticket_delivery_requests WHERE workspace_id = $1 AND idempotency_key = $2",
    )
    .bind(state.workspace_id.into_uuid())
    .bind(idempotency_key)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if let Some((existing_hash, created_at)) = existing {
        if existing_hash.as_slice() != request_hash.as_slice() {
            return Err(TicketingError::Conflict);
        }
        transaction.commit().await.map_err(TicketingError::sqlx)?;
        return Ok(TicketDeliveryRequestResponse {
            accepted: true,
            duplicate: true,
            requested_at: created_at,
        });
    }
    let last_requested = sqlx::query_scalar::<_, Option<OffsetDateTime>>(
        "SELECT last_delivery_requested_at FROM ticket_orders WHERE workspace_id = $1 AND id = $2",
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    let now = OffsetDateTime::now_utc();
    if last_requested
        .is_some_and(|value| (now - value).whole_seconds() < DELIVERY_RESEND_COOLDOWN_SECONDS)
    {
        return Err(TicketingError::Conflict);
    }
    sqlx::query(
        "INSERT INTO ticket_delivery_requests (workspace_id, ticket_order_id, idempotency_key, request_hash) VALUES ($1, $2, $3, $4)",
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order_id)
    .bind(idempotency_key)
    .bind(request_hash.as_slice())
    .execute(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    let updated = sqlx::query(
        "UPDATE ticket_orders SET last_delivery_requested_at = $3, delivery_request_count = delivery_request_count + 1 WHERE workspace_id = $1 AND id = $2",
    )
    .bind(state.workspace_id.into_uuid())
    .bind(order_id)
    .bind(now)
    .execute(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if updated.rows_affected() != 1 {
        return Err(TicketingError::Unexpected);
    }
    let rows = load_wallet_rows(&mut transaction, state.workspace_id, order_id).await?;
    let qr_not_before = ticket_qr_not_before(&order);
    let qr_expires_at = ticket_qr_expires_at(&order)?;
    let tickets = wallet_rows_payload(
        rows,
        order.event_id,
        qr_not_before,
        qr_expires_at,
        signing_key,
    )?;
    append_outbox(
        &mut transaction,
        state.workspace_id,
        "ticket.order.delivery_requested",
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
            "checkout_token": token,
            "tickets": tickets,
        }),
    )
    .await?;
    transaction.commit().await.map_err(TicketingError::sqlx)?;
    Ok(TicketDeliveryRequestResponse {
        accepted: true,
        duplicate: false,
        requested_at: now,
    })
}

async fn load_wallet_rows(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    order_id: Uuid,
) -> Result<Vec<TicketWalletRow>, TicketingError> {
    sqlx::query_as::<_, TicketWalletRow>(
        r#"
        SELECT
            pass.id AS pass_id,
            item.id AS order_item_id,
            ticket_type.slug AS ticket_type_slug,
            ticket_type.name AS ticket_type_name,
            pass.ticket_sequence AS sequence,
            pass.public_reference,
            pass.status,
            pass.holder_name,
            pass.holder_email,
            pass.redeemed_at
        FROM admission_passes AS pass
        JOIN ticket_order_items AS item
          ON item.workspace_id = pass.workspace_id AND item.id = pass.ticket_order_item_id
        JOIN ticket_types AS ticket_type
          ON ticket_type.workspace_id = item.workspace_id AND ticket_type.id = item.ticket_type_id
        WHERE item.workspace_id = $1 AND item.ticket_order_id = $2
        ORDER BY ticket_type.sort_order, item.id, pass.ticket_sequence
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(order_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)
}

fn wallet_rows_payload(
    rows: Vec<TicketWalletRow>,
    event_id: Uuid,
    qr_not_before: OffsetDateTime,
    qr_expires_at: OffsetDateTime,
    signing_key: &[u8; 32],
) -> Result<Vec<Value>, TicketingError> {
    rows.into_iter()
        .map(|row| {
            let qr_token = (row.status == "claimed")
                .then(|| {
                    encode_ticket_qr(
                        row.pass_id,
                        event_id,
                        &row.public_reference,
                        qr_not_before.unix_timestamp(),
                        qr_expires_at.unix_timestamp(),
                        signing_key,
                    )
                })
                .transpose()
                .map_err(|_| TicketingError::Unexpected)?;
            Ok(json!({
                "pass_id": row.pass_id,
                "order_item_id": row.order_item_id,
                "ticket_type_slug": row.ticket_type_slug,
                "ticket_type_name": row.ticket_type_name,
                "sequence": row.sequence,
                "public_reference": row.public_reference,
                "status": row.status,
                "holder_name": row.holder_name,
                "holder_email": row.holder_email,
                "redeemed_at": row.redeemed_at,
                "qr_token": qr_token,
                "qr_not_before": qr_not_before,
                "qr_expires_at": qr_expires_at,
            }))
        })
        .collect()
}

fn clone_order_row(row: &OrderRow) -> OrderRow {
    OrderRow {
        id: row.id,
        ticket_sale_id: row.ticket_sale_id,
        public_reference: row.public_reference.clone(),
        status: row.status.clone(),
        buyer_email: row.buyer_email.clone(),
        buyer_name: row.buyer_name.clone(),
        buyer_locale: row.buyer_locale.clone(),
        invoice_buyer_type: row.invoice_buyer_type.clone(),
        invoice_company_name: row.invoice_company_name.clone(),
        invoice_tax_id: row.invoice_tax_id.clone(),
        invoice_full_name: row.invoice_full_name.clone(),
        invoice_address_line1: row.invoice_address_line1.clone(),
        invoice_postal_code: row.invoice_postal_code.clone(),
        invoice_city: row.invoice_city.clone(),
        invoice_country_code: row.invoice_country_code.clone(),
        currency: row.currency.clone(),
        amount_gross_minor: row.amount_gross_minor,
        amount_net_minor: row.amount_net_minor,
        amount_vat_minor: row.amount_vat_minor,
        amount_refunded_minor: row.amount_refunded_minor,
        vat_rate_basis_points: row.vat_rate_basis_points,
        invoice_requested: row.invoice_requested,
        reservation_key: row.reservation_key.clone(),
        request_hash: row.request_hash.clone(),
        expires_at: row.expires_at,
        stripe_checkout_session_id: row.stripe_checkout_session_id.clone(),
        stripe_payment_intent_id: row.stripe_payment_intent_id.clone(),
        paid_at: row.paid_at,
        refunded_at: row.refunded_at,
        event_id: row.event_id,
        admission_pool_id: row.admission_pool_id,
        event_slug: row.event_slug.clone(),
        event_title: row.event_title.clone(),
        venue: row.venue.clone(),
        timezone: row.timezone.clone(),
        starts_at: row.starts_at,
        doors_at: row.doors_at,
        ends_at: row.ends_at,
    }
}

async fn configure_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    state: &TicketingState,
) -> Result<(), TicketingError> {
    let statement_ms = duration_milliseconds(state.operation_timeout)?;
    let lock_ms = duration_milliseconds(state.lock_timeout)?;
    sqlx::query(
        "SELECT set_config('statement_timeout', $1, true), set_config('lock_timeout', $2, true)",
    )
    .bind(format!("{statement_ms}ms"))
    .bind(format!("{lock_ms}ms"))
    .execute(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    Ok(())
}

fn duration_milliseconds(value: Duration) -> Result<u128, TicketingError> {
    let milliseconds = value.as_millis();
    if milliseconds == 0 || milliseconds > 2_147_483_647_u128 {
        return Err(TicketingError::Unexpected);
    }
    Ok(milliseconds)
}

async fn append_outbox(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    event_type: &str,
    request_id_value: Option<&str>,
    payload: Value,
) -> Result<(), TicketingError> {
    sqlx::query(
        r#"
        INSERT INTO outbox_events (
            workspace_id, event_type, event_version, payload, request_id
        ) VALUES ($1, $2, 1, $3, $4)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_type)
    .bind(payload)
    .bind(request_id_value)
    .execute(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn append_audit(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    actor_kind: &str,
    action: &str,
    target_type: &str,
    target_id: Uuid,
    request_id_value: Option<&str>,
    metadata: Value,
) -> Result<(), TicketingError> {
    sqlx::query(
        r#"
        INSERT INTO audit_events (
            workspace_id, actor_kind, action, target_type, target_id,
            request_id, metadata
        ) VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(actor_kind)
    .bind(action)
    .bind(target_type)
    .bind(target_id.to_string())
    .bind(request_id_value)
    .bind(metadata)
    .execute(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    Ok(())
}
