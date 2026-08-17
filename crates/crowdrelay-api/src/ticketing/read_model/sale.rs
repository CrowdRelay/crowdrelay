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

