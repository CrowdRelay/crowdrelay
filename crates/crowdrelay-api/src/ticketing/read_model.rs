#[derive(Debug, FromRow)]
#[allow(dead_code)]
struct ExistingSaleRow {
    id: Uuid,
    admission_pool_id: Uuid,
}

#[derive(Debug, FromRow)]
struct PoolCapacityRow {
    capacity: i32,
    issued_count: i32,
    reserved_count: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InventorySnapshot {
    sold: i32,
    reserved: i32,
    available: i32,
}

fn inventory_snapshot(
    capacity: i32,
    sold: i32,
    reserved: i32,
) -> Result<InventorySnapshot, TicketingError> {
    if capacity < 0 || sold < 0 || reserved < 0 {
        return Err(TicketingError::Unexpected);
    }
    let committed = sold
        .checked_add(reserved)
        .ok_or(TicketingError::Unexpected)?;
    if committed > capacity {
        return Err(TicketingError::Unexpected);
    }
    Ok(InventorySnapshot {
        sold,
        reserved,
        available: capacity - committed,
    })
}

#[derive(Debug, FromRow)]
struct OverviewTotalsRow {
    reserved_orders: i64,
    checkout_created_orders: i64,
    reserved_tickets: i64,
    paid_orders: i64,
    paid_tickets: i64,
    gross_sales_minor: i64,
    refunded_minor: i64,
}

const SALE_ROW_QUERY: &str = r#"
    SELECT
        sale.id,
        sale.event_id,
        sale.admission_pool_id,
        event.slug AS event_slug,
        event.title AS event_title,
        event.status AS event_status,
        event.venue,
        event.timezone,
        event.starts_at,
        event.doors_at,
        event.ends_at,
        sale.currency::text AS currency,
        sale.vat_rate_basis_points,
        pool.capacity AS capacity,
        pool.issued_count,
        pool.reserved_count,
        sale.max_per_order,
        sale.hold_seconds,
        sale.sales_open_at,
        sale.sales_close_at,
        sale.active
    FROM ticket_sales AS sale
    JOIN events AS event
      ON event.workspace_id = sale.workspace_id
     AND event.id = sale.event_id
    JOIN admission_pools AS pool
      ON pool.workspace_id = sale.workspace_id
     AND pool.id = sale.admission_pool_id
    WHERE sale.workspace_id = $1
      AND event.slug = $2
"#;

const ORDER_ROW_BASE: &str = r#"
    SELECT
        orders.id,
        orders.ticket_sale_id,
        orders.public_reference,
        orders.status,
        orders.buyer_email,
        orders.buyer_name,
        orders.buyer_locale,
        orders.invoice_buyer_type,
        orders.invoice_company_name,
        orders.invoice_tax_id,
        orders.invoice_full_name,
        orders.invoice_address_line1,
        orders.invoice_postal_code,
        orders.invoice_city,
        orders.invoice_country_code::text AS invoice_country_code,
        orders.currency::text AS currency,
        orders.amount_gross_minor,
        orders.amount_net_minor,
        orders.amount_vat_minor,
        orders.amount_refunded_minor,
        orders.vat_rate_basis_points,
        orders.invoice_requested,
        orders.reservation_key,
        orders.request_hash,
        orders.expires_at,
        orders.stripe_checkout_session_id,
        orders.stripe_payment_intent_id,
        orders.paid_at,
        orders.refunded_at,
        sale.event_id,
        sale.admission_pool_id,
        event.slug AS event_slug,
        event.title AS event_title,
        event.venue,
        event.timezone,
        event.starts_at,
        event.doors_at,
        event.ends_at
    FROM ticket_orders AS orders
    JOIN ticket_sales AS sale
      ON sale.workspace_id = orders.workspace_id
     AND sale.id = orders.ticket_sale_id
    JOIN events AS event
      ON event.workspace_id = sale.workspace_id
     AND event.id = sale.event_id
"#;

const ORDER_ROW_BY_ID_FOR_UPDATE: &str = r#"
    SELECT
        orders.id,
        orders.ticket_sale_id,
        orders.public_reference,
        orders.status,
        orders.buyer_email,
        orders.buyer_name,
        orders.buyer_locale,
        orders.invoice_buyer_type,
        orders.invoice_company_name,
        orders.invoice_tax_id,
        orders.invoice_full_name,
        orders.invoice_address_line1,
        orders.invoice_postal_code,
        orders.invoice_city,
        orders.invoice_country_code::text AS invoice_country_code,
        orders.currency::text AS currency,
        orders.amount_gross_minor,
        orders.amount_net_minor,
        orders.amount_vat_minor,
        orders.amount_refunded_minor,
        orders.vat_rate_basis_points,
        orders.invoice_requested,
        orders.reservation_key,
        orders.request_hash,
        orders.expires_at,
        orders.stripe_checkout_session_id,
        orders.stripe_payment_intent_id,
        orders.paid_at,
        orders.refunded_at,
        sale.event_id,
        sale.admission_pool_id,
        event.slug AS event_slug,
        event.title AS event_title,
        event.venue,
        event.timezone,
        event.starts_at,
        event.doors_at,
        event.ends_at
    FROM ticket_orders AS orders
    JOIN ticket_sales AS sale
      ON sale.workspace_id = orders.workspace_id
     AND sale.id = orders.ticket_sale_id
    JOIN events AS event
      ON event.workspace_id = sale.workspace_id
     AND event.id = sale.event_id
    WHERE orders.workspace_id = $1 AND orders.id = $2
    FOR UPDATE OF orders
"#;

const ORDER_ROW_BY_STRIPE_SESSION_FOR_UPDATE: &str = r#"
    SELECT
        orders.id,
        orders.ticket_sale_id,
        orders.public_reference,
        orders.status,
        orders.buyer_email,
        orders.buyer_name,
        orders.buyer_locale,
        orders.invoice_buyer_type,
        orders.invoice_company_name,
        orders.invoice_tax_id,
        orders.invoice_full_name,
        orders.invoice_address_line1,
        orders.invoice_postal_code,
        orders.invoice_city,
        orders.invoice_country_code::text AS invoice_country_code,
        orders.currency::text AS currency,
        orders.amount_gross_minor,
        orders.amount_net_minor,
        orders.amount_vat_minor,
        orders.amount_refunded_minor,
        orders.vat_rate_basis_points,
        orders.invoice_requested,
        orders.reservation_key,
        orders.request_hash,
        orders.expires_at,
        orders.stripe_checkout_session_id,
        orders.stripe_payment_intent_id,
        orders.paid_at,
        orders.refunded_at,
        sale.event_id,
        sale.admission_pool_id,
        event.slug AS event_slug,
        event.title AS event_title,
        event.venue,
        event.timezone,
        event.starts_at,
        event.doors_at,
        event.ends_at
    FROM ticket_orders AS orders
    JOIN ticket_sales AS sale
      ON sale.workspace_id = orders.workspace_id
     AND sale.id = orders.ticket_sale_id
    JOIN events AS event
      ON event.workspace_id = sale.workspace_id
     AND event.id = sale.event_id
    WHERE orders.workspace_id = $1
      AND orders.stripe_checkout_session_id = $2
    FOR UPDATE OF orders
"#;

const ORDER_ROW_BY_PAYMENT_INTENT_FOR_UPDATE: &str = r#"
    SELECT
        orders.id,
        orders.ticket_sale_id,
        orders.public_reference,
        orders.status,
        orders.buyer_email,
        orders.buyer_name,
        orders.buyer_locale,
        orders.invoice_buyer_type,
        orders.invoice_company_name,
        orders.invoice_tax_id,
        orders.invoice_full_name,
        orders.invoice_address_line1,
        orders.invoice_postal_code,
        orders.invoice_city,
        orders.invoice_country_code::text AS invoice_country_code,
        orders.currency::text AS currency,
        orders.amount_gross_minor,
        orders.amount_net_minor,
        orders.amount_vat_minor,
        orders.amount_refunded_minor,
        orders.vat_rate_basis_points,
        orders.invoice_requested,
        orders.reservation_key,
        orders.request_hash,
        orders.expires_at,
        orders.stripe_checkout_session_id,
        orders.stripe_payment_intent_id,
        orders.paid_at,
        orders.refunded_at,
        sale.event_id,
        sale.admission_pool_id,
        event.slug AS event_slug,
        event.title AS event_title,
        event.venue,
        event.timezone,
        event.starts_at,
        event.doors_at,
        event.ends_at
    FROM ticket_orders AS orders
    JOIN ticket_sales AS sale
      ON sale.workspace_id = orders.workspace_id
     AND sale.id = orders.ticket_sale_id
    JOIN events AS event
      ON event.workspace_id = sale.workspace_id
     AND event.id = sale.event_id
    WHERE orders.workspace_id = $1
      AND orders.stripe_payment_intent_id = $2
    FOR UPDATE OF orders
"#;

async fn lock_sale(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    event_slug: &str,
) -> Result<SaleRow, TicketingError> {
    let query = format!("{SALE_ROW_QUERY} FOR UPDATE OF sale, pool");
    sqlx::query_as::<_, SaleRow>(&query)
        .bind(workspace_id.into_uuid())
        .bind(event_slug)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(TicketingError::sqlx)?
        .ok_or(TicketingError::NotFound)
}

async fn cleanup_expired_reservations(
    state: &TicketingState,
    event_slug: &str,
) -> Result<(), TicketingError> {
    let mut transaction = state.pool.begin().await.map_err(TicketingError::sqlx)?;
    configure_transaction(&mut transaction, state).await?;
    let sale = lock_sale(&mut transaction, state.workspace_id, event_slug).await?;
    expire_active_reservations(
        &mut transaction,
        state.workspace_id,
        sale.id,
        sale.admission_pool_id,
    )
    .await?;
    transaction.commit().await.map_err(TicketingError::sqlx)?;
    Ok(())
}

async fn load_sale_view(
    state: &TicketingState,
    event_slug: &str,
    include_inactive: bool,
) -> Result<TicketSaleView, TicketingError> {
    cleanup_expired_reservations(state, event_slug).await?;
    let sale = sqlx::query_as::<_, SaleRow>(SALE_ROW_QUERY)
        .bind(state.workspace_id.into_uuid())
        .bind(event_slug)
        .fetch_optional(&state.pool)
        .await
        .map_err(TicketingError::sqlx)?
        .ok_or(TicketingError::NotFound)?;
    if !include_inactive
        && (!sale.active
            || sale.event_status != "published"
            || sale.starts_at <= OffsetDateTime::now_utc())
    {
        return Err(TicketingError::NotFound);
    }
    let ticket_types = sqlx::query_as::<_, TicketTypeRow>(
        r#"
        SELECT id, slug, name, description, price_gross_minor, capacity, sort_order, active
        FROM ticket_types
        WHERE workspace_id = $1
          AND ticket_sale_id = $2
          AND ($3 OR active)
        ORDER BY sort_order, id
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(sale.id)
    .bind(include_inactive)
    .fetch_all(&state.pool)
    .await
    .map_err(TicketingError::sqlx)?;
    build_sale_view(&state.pool, state.workspace_id, sale, ticket_types).await
}

async fn build_sale_view(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    sale: SaleRow,
    ticket_types: Vec<TicketTypeRow>,
) -> Result<TicketSaleView, TicketingError> {
    let sale_inventory = inventory_snapshot(sale.capacity, sale.issued_count, sale.reserved_count)?;
    let inventory_by_type = active_type_inventory_pool(pool, workspace_id, sale.id).await?;
    let sale_remaining = i64::from(sale_inventory.available);
    let mut type_views = Vec::with_capacity(ticket_types.len());
    for ticket_type in ticket_types {
        let inventory = inventory_by_type
            .get(&ticket_type.id)
            .copied()
            .unwrap_or_default();
        let committed = inventory.committed()?;
        let type_remaining = match ticket_type.capacity {
            Some(capacity) => {
                let capacity = i64::from(capacity);
                if committed > capacity {
                    return Err(TicketingError::Unexpected);
                }
                capacity - committed
            }
            None => sale_remaining,
        };
        let available = sale_remaining.min(type_remaining).min(i64::from(i32::MAX));
        type_views.push(TicketTypeView {
            id: ticket_type.id,
            slug: ticket_type.slug,
            name: ticket_type.name,
            description: ticket_type.description,
            price_gross_minor: ticket_type.price_gross_minor,
            capacity: ticket_type.capacity,
            sold: i32::try_from(inventory.sold).map_err(|_| TicketingError::Unexpected)?,
            reserved: i32::try_from(inventory.reserved).map_err(|_| TicketingError::Unexpected)?,
            available: i32::try_from(available).map_err(|_| TicketingError::Unexpected)?,
            sort_order: ticket_type.sort_order,
            active: ticket_type.active,
        });
    }
    let now = OffsetDateTime::now_utc();
    let hard_close_at = sale.sales_close_at.min(sale.starts_at);
    let latest_checkout_at = hard_close_at
        .checked_sub(time::Duration::seconds(i64::from(sale.hold_seconds)))
        .unwrap_or(sale.sales_open_at);
    let sales_state = if sale.event_status != "published" || now >= sale.starts_at {
        "event_unavailable"
    } else if !sale.active {
        "inactive"
    } else if now < sale.sales_open_at {
        "upcoming"
    } else if now > latest_checkout_at {
        "closed"
    } else if sale_inventory.available == 0 {
        "sold_out"
    } else {
        "open"
    };
    Ok(TicketSaleView {
        event_id: sale.event_id,
        event_slug: sale.event_slug,
        event_title: sale.event_title,
        event_status: sale.event_status,
        venue: sale.venue,
        timezone: sale.timezone,
        starts_at: sale.starts_at,
        currency: sale.currency,
        vat_rate_basis_points: sale.vat_rate_basis_points,
        capacity: sale.capacity,
        sold: sale_inventory.sold,
        reserved: sale_inventory.reserved,
        available: sale_inventory.available,
        max_per_order: sale.max_per_order,
        sales_open_at: sale.sales_open_at,
        sales_close_at: sale.sales_close_at,
        active: sale.active,
        sales_state,
        ticket_types: type_views,
    })
}

async fn load_admin_overview(
    state: &TicketingState,
    event_slug: &str,
) -> Result<AdminTicketingOverview, TicketingError> {
    let sale = load_sale_view(state, event_slug, true).await?;
    let sale_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT sale.id
        FROM ticket_sales AS sale
        JOIN events AS event
          ON event.workspace_id = sale.workspace_id
         AND event.id = sale.event_id
        WHERE sale.workspace_id = $1 AND event.slug = $2
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(event_slug)
    .fetch_one(&state.pool)
    .await
    .map_err(TicketingError::sqlx)?;
    let totals = sqlx::query_as::<_, OverviewTotalsRow>(
        r#"
        WITH order_totals AS (
            SELECT
                orders.id,
                orders.status,
                orders.expires_at,
                orders.amount_gross_minor,
                orders.amount_refunded_minor,
                COALESCE(sum(item.quantity), 0)::bigint AS ticket_count
            FROM ticket_orders AS orders
            LEFT JOIN ticket_order_items AS item
              ON item.workspace_id = orders.workspace_id
             AND item.ticket_order_id = orders.id
            WHERE orders.workspace_id = $1
              AND orders.ticket_sale_id = $2
            GROUP BY
                orders.id,
                orders.status,
                orders.expires_at,
                orders.amount_gross_minor,
                orders.amount_refunded_minor
        )
        SELECT
            count(*) FILTER (
                WHERE status IN ('reserved', 'checkout_created')
                  AND expires_at > now()
            )::bigint AS reserved_orders,
            count(*) FILTER (
                WHERE status = 'checkout_created'
                  AND expires_at > now()
            )::bigint AS checkout_created_orders,
            COALESCE(sum(ticket_count) FILTER (
                WHERE status IN ('reserved', 'checkout_created')
                  AND expires_at > now()
            ), 0)::bigint AS reserved_tickets,
            count(*) FILTER (
                WHERE status IN ('paid', 'partially_refunded', 'refunded')
            )::bigint AS paid_orders,
            COALESCE(sum(ticket_count) FILTER (
                WHERE status IN ('paid', 'partially_refunded', 'refunded')
            ), 0)::bigint AS paid_tickets,
            COALESCE(sum(amount_gross_minor) FILTER (
                WHERE status IN ('paid', 'partially_refunded', 'refunded')
            ), 0)::bigint AS gross_sales_minor,
            COALESCE(sum(amount_refunded_minor), 0)::bigint AS refunded_minor
        FROM order_totals
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(sale_id)
    .fetch_one(&state.pool)
    .await
    .map_err(TicketingError::sqlx)?;
    let recent_rows = sqlx::query_as::<_, OrderRow>(&format!(
        "{ORDER_ROW_BASE} WHERE orders.workspace_id = $1 AND orders.ticket_sale_id = $2 ORDER BY orders.created_at DESC, orders.id DESC LIMIT 50"
    ))
    .bind(state.workspace_id.into_uuid())
    .bind(sale_id)
    .fetch_all(&state.pool)
    .await
    .map_err(TicketingError::sqlx)?;
    let mut recent_orders = Vec::with_capacity(recent_rows.len());
    for row in recent_rows {
        recent_orders.push(load_order_view_pool(&state.pool, state.workspace_id, row).await?);
    }
    Ok(AdminTicketingOverview {
        sale,
        reserved_orders: totals.reserved_orders,
        checkout_created_orders: totals.checkout_created_orders,
        reserved_tickets: totals.reserved_tickets,
        paid_orders: totals.paid_orders,
        paid_tickets: totals.paid_tickets,
        gross_sales_minor: totals.gross_sales_minor,
        refunded_minor: totals.refunded_minor,
        recent_orders,
    })
}

async fn load_order_by_token(
    state: &TicketingState,
    order_id: Uuid,
    checkout_token: &str,
) -> Result<TicketOrderView, TicketingError> {
    if !valid_checkout_token(checkout_token) {
        return Err(TicketingError::NotFound);
    }
    let query = format!(
        "{ORDER_ROW_BASE} WHERE orders.workspace_id = $1 AND orders.id = $2 AND orders.checkout_token_hash = digest($3, 'sha256')"
    );
    let row = sqlx::query_as::<_, OrderRow>(&query)
        .bind(state.workspace_id.into_uuid())
        .bind(order_id)
        .bind(checkout_token)
        .fetch_optional(&state.pool)
        .await
        .map_err(TicketingError::sqlx)?
        .ok_or(TicketingError::NotFound)?;
    load_order_view_pool(&state.pool, state.workspace_id, row).await
}

async fn load_order_row_by_reservation_key(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    reservation_key: &str,
) -> Result<Option<OrderRow>, TicketingError> {
    let query = format!(
        "{ORDER_ROW_BASE} WHERE orders.workspace_id = $1 AND orders.reservation_key = $2 FOR UPDATE OF orders"
    );
    sqlx::query_as::<_, OrderRow>(&query)
        .bind(workspace_id.into_uuid())
        .bind(reservation_key)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(TicketingError::sqlx)
}

async fn load_order_row_by_id(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    order_id: Uuid,
) -> Result<OrderRow, TicketingError> {
    sqlx::query_as::<_, OrderRow>(ORDER_ROW_BY_ID_FOR_UPDATE)
        .bind(workspace_id.into_uuid())
        .bind(order_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(TicketingError::sqlx)?
        .ok_or(TicketingError::NotFound)
}

async fn expire_active_reservations(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    sale_id: Uuid,
    admission_pool_id: Uuid,
) -> Result<i32, TicketingError> {
    let released_quantity = sqlx::query_scalar::<_, i64>(
        r#"
        WITH expired_orders AS MATERIALIZED (
            SELECT id
            FROM ticket_orders
            WHERE workspace_id = $1
              AND ticket_sale_id = $2
              AND status IN ('reserved', 'checkout_created')
              AND expires_at <= now()
            FOR UPDATE
        ),
        released_orders AS (
            UPDATE ticket_orders AS orders
            SET status = 'expired', released_at = now()
            FROM expired_orders
            WHERE orders.workspace_id = $1
              AND orders.id = expired_orders.id
            RETURNING orders.id
        )
        SELECT COALESCE(sum(item.quantity), 0)::bigint
        FROM released_orders
        JOIN ticket_order_items AS item
          ON item.workspace_id = $1
         AND item.ticket_order_id = released_orders.id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(sale_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    let released_quantity =
        i32::try_from(released_quantity).map_err(|_| TicketingError::Unexpected)?;
    if released_quantity == 0 {
        return Ok(0);
    }
    let released = sqlx::query(
        r#"
        UPDATE admission_pools
        SET reserved_count = reserved_count - $3
        WHERE workspace_id = $1 AND id = $2
          AND reserved_count >= $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(admission_pool_id)
    .bind(released_quantity)
    .execute(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if released.rows_affected() != 1 {
        return Err(TicketingError::Unexpected);
    }
    Ok(released_quantity)
}

async fn order_ticket_count(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    order_id: Uuid,
) -> Result<i32, TicketingError> {
    let quantity = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COALESCE(sum(quantity), 0)::bigint
        FROM ticket_order_items
        WHERE workspace_id = $1 AND ticket_order_id = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(order_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    let quantity = i32::try_from(quantity).map_err(|_| TicketingError::Unexpected)?;
    if quantity <= 0 {
        return Err(TicketingError::Unexpected);
    }
    Ok(quantity)
}

async fn release_order_reservation(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    order: &OrderRow,
    status: &str,
) -> Result<(), TicketingError> {
    if !matches!(status, "expired" | "cancelled" | "payment_failed") {
        return Err(TicketingError::Unexpected);
    }
    let quantity = order_ticket_count(transaction, workspace_id, order.id).await?;
    let released = sqlx::query(
        r#"
        UPDATE admission_pools
        SET reserved_count = reserved_count - $3
        WHERE workspace_id = $1 AND id = $2
          AND reserved_count >= $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(order.admission_pool_id)
    .bind(quantity)
    .execute(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if released.rows_affected() != 1 {
        return Err(TicketingError::Unexpected);
    }
    let updated_order = sqlx::query(
        r#"
        UPDATE ticket_orders
        SET status = $3, released_at = now()
        WHERE workspace_id = $1 AND id = $2
          AND status IN ('reserved', 'checkout_created')
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(order.id)
    .bind(status)
    .execute(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)?;
    if updated_order.rows_affected() != 1 {
        return Err(TicketingError::Conflict);
    }
    Ok(())
}

async fn load_order_items(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    order_id: Uuid,
) -> Result<Vec<OrderItemRow>, TicketingError> {
    sqlx::query_as::<_, OrderItemRow>(
        r#"
        SELECT
            item.id,
            item.ticket_type_id,
            ticket_type.slug AS ticket_type_slug,
            ticket_type.name AS ticket_type_name,
            item.quantity,
            item.unit_gross_minor,
            item.unit_net_minor,
            item.unit_vat_minor,
            item.total_gross_minor,
            item.total_net_minor,
            item.total_vat_minor
        FROM ticket_order_items AS item
        JOIN ticket_types AS ticket_type
          ON ticket_type.workspace_id = item.workspace_id
         AND ticket_type.id = item.ticket_type_id
        WHERE item.workspace_id = $1 AND item.ticket_order_id = $2
        ORDER BY ticket_type.sort_order, item.id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(order_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)
}

async fn load_order_items_pool(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    order_id: Uuid,
) -> Result<Vec<OrderItemRow>, TicketingError> {
    sqlx::query_as::<_, OrderItemRow>(
        r#"
        SELECT
            item.id,
            item.ticket_type_id,
            ticket_type.slug AS ticket_type_slug,
            ticket_type.name AS ticket_type_name,
            item.quantity,
            item.unit_gross_minor,
            item.unit_net_minor,
            item.unit_vat_minor,
            item.total_gross_minor,
            item.total_net_minor,
            item.total_vat_minor
        FROM ticket_order_items AS item
        JOIN ticket_types AS ticket_type
          ON ticket_type.workspace_id = item.workspace_id
         AND ticket_type.id = item.ticket_type_id
        WHERE item.workspace_id = $1 AND item.ticket_order_id = $2
        ORDER BY ticket_type.sort_order, item.id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(order_id)
    .fetch_all(pool)
    .await
    .map_err(TicketingError::sqlx)
}

async fn load_issued_tickets(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    order_id: Uuid,
) -> Result<Vec<IssuedTicketRow>, TicketingError> {
    sqlx::query_as::<_, IssuedTicketRow>(
        r#"
        SELECT
            pass.id AS pass_id,
            item.id AS order_item_id,
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
        WHERE item.workspace_id = $1 AND item.ticket_order_id = $2
        ORDER BY item.id, pass.ticket_sequence
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(order_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(TicketingError::sqlx)
}

async fn load_issued_tickets_pool(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    order_id: Uuid,
) -> Result<Vec<IssuedTicketRow>, TicketingError> {
    sqlx::query_as::<_, IssuedTicketRow>(
        r#"
        SELECT
            pass.id AS pass_id,
            item.id AS order_item_id,
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
        WHERE item.workspace_id = $1 AND item.ticket_order_id = $2
        ORDER BY item.id, pass.ticket_sequence
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(order_id)
    .fetch_all(pool)
    .await
    .map_err(TicketingError::sqlx)
}

async fn load_order_view_for_row(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    row: OrderRow,
) -> Result<TicketOrderView, TicketingError> {
    let items = load_order_items(transaction, workspace_id, row.id).await?;
    let tickets = load_issued_tickets(transaction, workspace_id, row.id).await?;
    Ok(order_view(row, items, tickets))
}

async fn load_order_view_pool(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    row: OrderRow,
) -> Result<TicketOrderView, TicketingError> {
    let items = load_order_items_pool(pool, workspace_id, row.id).await?;
    let tickets = load_issued_tickets_pool(pool, workspace_id, row.id).await?;
    Ok(order_view(row, items, tickets))
}

fn order_view(
    row: OrderRow,
    items: Vec<OrderItemRow>,
    tickets: Vec<IssuedTicketRow>,
) -> TicketOrderView {
    TicketOrderView {
        order_id: row.id,
        public_reference: row.public_reference,
        event_slug: row.event_slug,
        event_title: row.event_title,
        venue: row.venue,
        timezone: row.timezone,
        starts_at: row.starts_at,
        status: row.status,
        buyer_email_masked: mask_email(&row.buyer_email),
        buyer_name: row.buyer_name,
        currency: row.currency,
        amount_gross_minor: row.amount_gross_minor,
        amount_net_minor: row.amount_net_minor,
        amount_vat_minor: row.amount_vat_minor,
        amount_refunded_minor: row.amount_refunded_minor,
        vat_rate_basis_points: row.vat_rate_basis_points,
        invoice_requested: row.invoice_requested,
        expires_at: row.expires_at,
        paid_at: row.paid_at,
        refunded_at: row.refunded_at,
        items: items
            .into_iter()
            .map(|item| TicketOrderItemView {
                id: item.id,
                ticket_type_slug: item.ticket_type_slug,
                ticket_type_name: item.ticket_type_name,
                quantity: item.quantity,
                unit_gross_minor: item.unit_gross_minor,
                unit_net_minor: item.unit_net_minor,
                unit_vat_minor: item.unit_vat_minor,
                total_gross_minor: item.total_gross_minor,
                total_net_minor: item.total_net_minor,
                total_vat_minor: item.total_vat_minor,
            })
            .collect(),
        tickets: tickets
            .into_iter()
            .map(|ticket| IssuedTicketView {
                pass_id: ticket.pass_id,
                order_item_id: ticket.order_item_id,
                sequence: ticket.sequence,
                public_reference: ticket.public_reference,
                status: ticket.status,
                holder_name: ticket.holder_name,
                holder_email_masked: mask_email(&ticket.holder_email),
                redeemed_at: ticket.redeemed_at,
            })
            .collect(),
    }
}

const TYPE_INVENTORY_QUERY: &str = r#"
    WITH inventory AS (
        SELECT
            item.ticket_type_id,
            item.quantity::bigint AS reserved,
            0::bigint AS sold
        FROM ticket_order_items AS item
        JOIN ticket_orders AS orders
          ON orders.workspace_id = item.workspace_id
         AND orders.id = item.ticket_order_id
        WHERE orders.workspace_id = $1
          AND orders.ticket_sale_id = $2
          AND (
              orders.status IN ('reserved', 'checkout_created')
              AND orders.expires_at > now()
          )

        UNION ALL

        SELECT
            item.ticket_type_id,
            0::bigint AS reserved,
            count(pass.id)::bigint AS sold
        FROM ticket_order_items AS item
        JOIN ticket_orders AS orders
          ON orders.workspace_id = item.workspace_id
         AND orders.id = item.ticket_order_id
        JOIN admission_passes AS pass
          ON pass.workspace_id = item.workspace_id
         AND pass.ticket_order_item_id = item.id
        WHERE orders.workspace_id = $1
          AND orders.ticket_sale_id = $2
          AND orders.status IN ('paid', 'partially_refunded', 'refunded')
          AND pass.status IN ('issued', 'claimed', 'redeemed')
        GROUP BY item.ticket_type_id
    )
    SELECT
        ticket_type_id,
        COALESCE(sum(reserved), 0)::bigint AS reserved,
        COALESCE(sum(sold), 0)::bigint AS sold
    FROM inventory
    GROUP BY ticket_type_id
"#;

fn collect_type_inventory(
    rows: Vec<TypeInventoryRow>,
) -> Result<HashMap<Uuid, TypeInventory>, TicketingError> {
    let mut inventory = HashMap::with_capacity(rows.len());
    for row in rows {
        if row.reserved < 0 || row.sold < 0 {
            return Err(TicketingError::Unexpected);
        }
        inventory.insert(
            row.ticket_type_id,
            TypeInventory {
                reserved: row.reserved,
                sold: row.sold,
            },
        );
    }
    Ok(inventory)
}

async fn active_type_inventory(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    sale_id: Uuid,
) -> Result<HashMap<Uuid, TypeInventory>, TicketingError> {
    let rows = sqlx::query_as::<_, TypeInventoryRow>(TYPE_INVENTORY_QUERY)
        .bind(workspace_id.into_uuid())
        .bind(sale_id)
        .fetch_all(&mut **transaction)
        .await
        .map_err(TicketingError::sqlx)?;
    collect_type_inventory(rows)
}

async fn active_type_inventory_pool(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    sale_id: Uuid,
) -> Result<HashMap<Uuid, TypeInventory>, TicketingError> {
    let rows = sqlx::query_as::<_, TypeInventoryRow>(TYPE_INVENTORY_QUERY)
        .bind(workspace_id.into_uuid())
        .bind(sale_id)
        .fetch_all(pool)
        .await
        .map_err(TicketingError::sqlx)?;
    collect_type_inventory(rows)
}
