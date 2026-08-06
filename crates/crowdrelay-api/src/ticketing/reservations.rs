async fn configure_sale_inner(
    state: &TicketingState,
    event_slug: &str,
    request: &ConfigureTicketSaleRequest,
    ticket_types: &[NormalizedTicketType],
    request_id_value: Option<&str>,
) -> Result<(), TicketingError> {
    let mut transaction = state.pool.begin().await.map_err(TicketingError::sqlx)?;
    configure_transaction(&mut transaction, state).await?;

    let (event_id, event_starts_at) = sqlx::query_as::<_, (Uuid, OffsetDateTime)>(
        r#"
        SELECT id, starts_at
        FROM events
        WHERE workspace_id = $1
          AND slug = $2
          AND status = 'published'
          AND starts_at > now()
        FOR UPDATE
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(event_slug)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?
    .ok_or(TicketingError::NotFound)?;
    if request.sales_close_at > event_starts_at {
        return Err(TicketingError::Invalid);
    }

    let existing_sale = sqlx::query_as::<_, ExistingSaleRow>(
        r#"
        SELECT id, admission_pool_id
        FROM ticket_sales
        WHERE workspace_id = $1 AND event_id = $2
        FOR UPDATE
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(event_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;

    let pool_id = if let Some(sale) = &existing_sale {
        let (issued_count, reserved_count) = sqlx::query_as::<_, (i32, i32)>(
            r#"
            SELECT issued_count, reserved_count
            FROM admission_pools
            WHERE workspace_id = $1 AND id = $2
            FOR UPDATE
            "#,
        )
        .bind(state.workspace_id.into_uuid())
        .bind(sale.admission_pool_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(TicketingError::sqlx)?;
        if issued_count.saturating_add(reserved_count) > request.capacity {
            return Err(TicketingError::Conflict);
        }
        let updated_pool = sqlx::query(
            r#"
            UPDATE admission_pools
            SET capacity = $3, active = $4, name = 'Paid tickets'
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(state.workspace_id.into_uuid())
        .bind(sale.admission_pool_id)
        .bind(request.capacity)
        .bind(request.active)
        .execute(&mut *transaction)
        .await
        .map_err(TicketingError::sqlx)?;
        if updated_pool.rows_affected() != 1 {
            return Err(TicketingError::Unexpected);
        }
        sale.admission_pool_id
    } else {
        sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO admission_pools (
                workspace_id, event_id, slug, name, capacity, active
            ) VALUES ($1, $2, 'paid-tickets', 'Paid tickets', $3, $4)
            ON CONFLICT (workspace_id, event_id, slug) DO UPDATE
            SET capacity = EXCLUDED.capacity,
                active = EXCLUDED.active,
                name = EXCLUDED.name
            WHERE admission_pools.issued_count + admission_pools.reserved_count
                <= EXCLUDED.capacity
            RETURNING id
            "#,
        )
        .bind(state.workspace_id.into_uuid())
        .bind(event_id)
        .bind(request.capacity)
        .bind(request.active)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(TicketingError::sqlx)?
        .ok_or(TicketingError::Conflict)?
    };

    let sale_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO ticket_sales (
            workspace_id, event_id, admission_pool_id, currency,
            vat_rate_basis_points, capacity, max_per_order, hold_seconds,
            sales_open_at, sales_close_at, active
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        ON CONFLICT (workspace_id, event_id) DO UPDATE
        SET admission_pool_id = EXCLUDED.admission_pool_id,
            currency = EXCLUDED.currency,
            vat_rate_basis_points = EXCLUDED.vat_rate_basis_points,
            capacity = EXCLUDED.capacity,
            max_per_order = EXCLUDED.max_per_order,
            hold_seconds = EXCLUDED.hold_seconds,
            sales_open_at = EXCLUDED.sales_open_at,
            sales_close_at = EXCLUDED.sales_close_at,
            active = EXCLUDED.active
        RETURNING id
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(event_id)
    .bind(pool_id)
    .bind(request.currency.trim().to_ascii_uppercase())
    .bind(request.vat_rate_basis_points)
    .bind(request.capacity)
    .bind(request.max_per_order)
    .bind(request.hold_seconds)
    .bind(request.sales_open_at)
    .bind(request.sales_close_at)
    .bind(request.active)
    .fetch_one(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;

    let configured_slugs: Vec<String> = ticket_types.iter().map(|item| item.slug.clone()).collect();
    sqlx::query(
        r#"
        UPDATE ticket_types
        SET active = false
        WHERE workspace_id = $1
          AND ticket_sale_id = $2
          AND NOT (slug = ANY($3))
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(sale_id)
    .bind(&configured_slugs)
    .execute(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;

    for ticket_type in ticket_types {
        sqlx::query(
            r#"
            INSERT INTO ticket_types (
                workspace_id, ticket_sale_id, slug, name, description,
                price_gross_minor, capacity, sort_order, active
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT (workspace_id, ticket_sale_id, slug) DO UPDATE
            SET name = EXCLUDED.name,
                description = EXCLUDED.description,
                price_gross_minor = EXCLUDED.price_gross_minor,
                capacity = EXCLUDED.capacity,
                sort_order = EXCLUDED.sort_order,
                active = EXCLUDED.active
            "#,
        )
        .bind(state.workspace_id.into_uuid())
        .bind(sale_id)
        .bind(&ticket_type.slug)
        .bind(&ticket_type.name)
        .bind(&ticket_type.description)
        .bind(ticket_type.price_gross_minor)
        .bind(ticket_type.capacity)
        .bind(ticket_type.sort_order)
        .bind(ticket_type.active)
        .execute(&mut *transaction)
        .await
        .map_err(TicketingError::sqlx)?;
    }

    let inventory = active_type_inventory(&mut transaction, state.workspace_id, sale_id).await?;
    let configured_rows = sqlx::query_as::<_, TicketTypeRow>(
        r#"
        SELECT id, slug, name, description, price_gross_minor, capacity, sort_order, active
        FROM ticket_types
        WHERE workspace_id = $1 AND ticket_sale_id = $2
        FOR SHARE
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(sale_id)
    .fetch_all(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    for ticket_type in &configured_rows {
        let Some(capacity) = ticket_type.capacity else {
            continue;
        };
        let committed = inventory
            .get(&ticket_type.id)
            .copied()
            .unwrap_or_default()
            .committed()?;
        if committed > i64::from(capacity) {
            return Err(TicketingError::Conflict);
        }
    }

    append_audit(
        &mut transaction,
        state.workspace_id,
        "service",
        "ticket_sale.configured",
        "ticket_sale",
        sale_id,
        request_id_value,
        json!({
            "event_id": event_id,
            "capacity": request.capacity,
            "currency": request.currency.trim().to_ascii_uppercase(),
            "vat_rate_basis_points": request.vat_rate_basis_points,
            "ticket_type_count": ticket_types.len(),
            "active": request.active,
        }),
    )
    .await?;

    transaction.commit().await.map_err(TicketingError::sqlx)?;
    Ok(())
}

async fn reserve_order_inner(
    state: &TicketingState,
    event_slug: &str,
    idempotency_key: &str,
    request_id_value: Option<&str>,
    reservation: &NormalizedReservation,
    checkout_token_key: &[u8; 32],
) -> Result<TicketReservationResponse, TicketingError> {
    let mut transaction = state.pool.begin().await.map_err(TicketingError::sqlx)?;
    configure_transaction(&mut transaction, state).await?;

    if let Some(existing) =
        load_order_row_by_reservation_key(&mut transaction, state.workspace_id, idempotency_key)
            .await?
    {
        if existing.event_slug != event_slug
            || existing.request_hash.as_slice() != reservation.request_hash.as_slice()
        {
            return Err(TicketingError::Conflict);
        }
        let checkout_token =
            derive_checkout_token(checkout_token_key, existing.id, &existing.reservation_key)?;
        let order = load_order_view_for_row(&mut transaction, state.workspace_id, existing).await?;
        transaction.commit().await.map_err(TicketingError::sqlx)?;
        return Ok(TicketReservationResponse {
            checkout_token,
            order,
        });
    }

    let sale = lock_sale(&mut transaction, state.workspace_id, event_slug).await?;
    let now = OffsetDateTime::now_utc();
    if sale.event_status != "published"
        || now >= sale.starts_at
        || !sale.active
        || now < sale.sales_open_at
        || now >= sale.sales_close_at
    {
        return Err(TicketingError::Conflict);
    }
    if reservation.total_quantity > sale.max_per_order {
        return Err(TicketingError::Invalid);
    }

    let expired_quantity = expire_active_reservations(
        &mut transaction,
        state.workspace_id,
        sale.id,
        sale.admission_pool_id,
    )
    .await?;
    let current_reserved_count = sale
        .reserved_count
        .checked_sub(expired_quantity)
        .ok_or(TicketingError::Unexpected)?;
    if sale
        .issued_count
        .saturating_add(current_reserved_count)
        .saturating_add(reservation.total_quantity)
        > sale.capacity
    {
        return Err(TicketingError::Conflict);
    }

    let slugs: Vec<String> = reservation
        .items
        .iter()
        .map(|(slug, _)| slug.clone())
        .collect();
    let ticket_type_rows = sqlx::query_as::<_, TicketTypeRow>(
        r#"
        SELECT id, slug, name, description, price_gross_minor, capacity, sort_order, active
        FROM ticket_types
        WHERE workspace_id = $1
          AND ticket_sale_id = $2
          AND slug = ANY($3)
        ORDER BY sort_order, id
        FOR SHARE
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(sale.id)
    .bind(&slugs)
    .fetch_all(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if ticket_type_rows.len() != reservation.items.len()
        || ticket_type_rows.iter().any(|item| !item.active)
    {
        return Err(TicketingError::NotFound);
    }

    let inventory_by_type =
        active_type_inventory(&mut transaction, state.workspace_id, sale.id).await?;
    let quantity_by_slug: HashMap<&str, i32> = reservation
        .items
        .iter()
        .map(|(slug, quantity)| (slug.as_str(), *quantity))
        .collect();

    let mut prepared = Vec::with_capacity(ticket_type_rows.len());
    let mut amount_gross_minor = 0_i64;
    let mut amount_net_minor = 0_i64;
    let mut amount_vat_minor = 0_i64;
    for ticket_type in ticket_type_rows {
        let quantity = quantity_by_slug
            .get(ticket_type.slug.as_str())
            .copied()
            .ok_or(TicketingError::Invalid)?;
        let committed = inventory_by_type
            .get(&ticket_type.id)
            .copied()
            .unwrap_or_default()
            .committed()?;
        let requested_commitment = committed
            .checked_add(i64::from(quantity))
            .ok_or(TicketingError::Unexpected)?;
        if ticket_type
            .capacity
            .is_some_and(|capacity| requested_commitment > i64::from(capacity))
        {
            return Err(TicketingError::Conflict);
        }
        let unit_gross_minor = ticket_type.price_gross_minor;
        let (unit_net_minor, unit_vat_minor) =
            split_gross(unit_gross_minor, sale.vat_rate_basis_points)?;
        let total_gross_minor = unit_gross_minor
            .checked_mul(i64::from(quantity))
            .ok_or(TicketingError::Invalid)?;
        let (total_net_minor, total_vat_minor) =
            split_gross(total_gross_minor, sale.vat_rate_basis_points)?;
        amount_gross_minor = amount_gross_minor
            .checked_add(total_gross_minor)
            .ok_or(TicketingError::Invalid)?;
        amount_net_minor = amount_net_minor
            .checked_add(total_net_minor)
            .ok_or(TicketingError::Invalid)?;
        amount_vat_minor = amount_vat_minor
            .checked_add(total_vat_minor)
            .ok_or(TicketingError::Invalid)?;
        prepared.push(PreparedOrderItem {
            id: Uuid::now_v7(),
            ticket_type,
            quantity,
            unit_gross_minor,
            unit_net_minor,
            unit_vat_minor,
            total_gross_minor,
            total_net_minor,
            total_vat_minor,
        });
    }
    if amount_net_minor + amount_vat_minor != amount_gross_minor {
        return Err(TicketingError::Unexpected);
    }

    let hold_expires_at = now
        .checked_add(time::Duration::seconds(i64::from(sale.hold_seconds)))
        .ok_or(TicketingError::Unexpected)?;
    let hard_close_at = sale.sales_close_at.min(sale.starts_at);
    if hold_expires_at > hard_close_at {
        return Err(TicketingError::Conflict);
    }

    let order_id = Uuid::now_v7();
    let public_reference = order_public_reference(order_id);
    let checkout_token = derive_checkout_token(checkout_token_key, order_id, idempotency_key)?;
    let checkout_token_hash: [u8; 32] = Sha256::digest(checkout_token.as_bytes()).into();
    let expires_at = hold_expires_at;

    let invoice = reservation.invoice_details.as_ref();
    sqlx::query(
        r#"
        INSERT INTO ticket_orders (
            id, workspace_id, ticket_sale_id, public_reference, status,
            buyer_email, buyer_name, buyer_locale,
            invoice_buyer_type, invoice_company_name, invoice_tax_id,
            invoice_full_name, invoice_address_line1, invoice_postal_code,
            invoice_city, invoice_country_code,
            currency, amount_gross_minor, amount_net_minor, amount_vat_minor,
            vat_rate_basis_points, invoice_requested, reservation_key,
            request_hash, checkout_token_hash, expires_at
        ) VALUES (
            $1, $2, $3, $4, 'reserved', $5, $6, $7,
            $8, $9, $10, $11, $12, $13, $14, $15,
            $16, $17, $18, $19, $20, $21, $22, $23, $24, $25
        )
        "#,
    )
    .bind(order_id)
    .bind(state.workspace_id.into_uuid())
    .bind(sale.id)
    .bind(&public_reference)
    .bind(reservation.buyer_email.as_str())
    .bind(&reservation.buyer_name)
    .bind(&reservation.buyer_locale)
    .bind(invoice.map(|value| value.buyer_type.as_str()))
    .bind(invoice.and_then(|value| value.company_name.as_deref()))
    .bind(invoice.and_then(|value| value.tax_id.as_deref()))
    .bind(invoice.and_then(|value| value.full_name.as_deref()))
    .bind(invoice.map(|value| value.address_line1.as_str()))
    .bind(invoice.map(|value| value.postal_code.as_str()))
    .bind(invoice.map(|value| value.city.as_str()))
    .bind(invoice.map(|value| value.country_code.as_str()))
    .bind(&sale.currency)
    .bind(amount_gross_minor)
    .bind(amount_net_minor)
    .bind(amount_vat_minor)
    .bind(sale.vat_rate_basis_points)
    .bind(reservation.invoice_requested)
    .bind(idempotency_key)
    .bind(reservation.request_hash.as_slice())
    .bind(checkout_token_hash.as_slice())
    .bind(expires_at)
    .execute(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;

    for item in &prepared {
        sqlx::query(
            r#"
            INSERT INTO ticket_order_items (
                id, workspace_id, ticket_order_id, ticket_type_id, quantity,
                unit_gross_minor, unit_net_minor, unit_vat_minor,
                total_gross_minor, total_net_minor, total_vat_minor
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            "#,
        )
        .bind(item.id)
        .bind(state.workspace_id.into_uuid())
        .bind(order_id)
        .bind(item.ticket_type.id)
        .bind(item.quantity)
        .bind(item.unit_gross_minor)
        .bind(item.unit_net_minor)
        .bind(item.unit_vat_minor)
        .bind(item.total_gross_minor)
        .bind(item.total_net_minor)
        .bind(item.total_vat_minor)
        .execute(&mut *transaction)
        .await
        .map_err(TicketingError::sqlx)?;
    }

    let reserved = sqlx::query(
        r#"
        UPDATE admission_pools
        SET reserved_count = reserved_count + $3
        WHERE workspace_id = $1 AND id = $2
          AND issued_count + reserved_count + $3 <= capacity
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(sale.admission_pool_id)
    .bind(reservation.total_quantity)
    .execute(&mut *transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if reserved.rows_affected() != 1 {
        return Err(TicketingError::Conflict);
    }

    append_audit(
        &mut transaction,
        state.workspace_id,
        "service",
        "ticket_order.reserved",
        "ticket_order",
        order_id,
        request_id_value,
        json!({
            "event_id": sale.event_id,
            "quantity": reservation.total_quantity,
            "amount_gross_minor": amount_gross_minor,
            "currency": sale.currency,
            "expires_at": expires_at,
        }),
    )
    .await?;

    let order_row = load_order_row_by_id(&mut transaction, state.workspace_id, order_id).await?;
    let order = load_order_view_for_row(&mut transaction, state.workspace_id, order_row).await?;
    transaction.commit().await.map_err(TicketingError::sqlx)?;
    Ok(TicketReservationResponse {
        checkout_token,
        order,
    })
}

// Binds a reserved order to exactly one Stripe Checkout Session.
