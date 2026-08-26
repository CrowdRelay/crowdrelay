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
    let recent_orders = load_order_views_pool_batch(&state.pool, state.workspace_id, recent_rows).await?;
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

#[derive(FromRow)]
struct BatchOrderItemRow {
    ticket_order_id: Uuid,
    #[sqlx(flatten)]
    item: OrderItemRow,
}

#[derive(FromRow)]
struct BatchIssuedTicketRow {
    ticket_order_id: Uuid,
    #[sqlx(flatten)]
    ticket: IssuedTicketRow,
}

/// Loads a page of order views with TWO queries total instead of two per
/// order: the staff overview renders up to 50 recent orders, so the per-order
/// loop serialized ~100 sequential pool round trips per dashboard render.
async fn load_order_views_pool_batch(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    rows: Vec<OrderRow>,
) -> Result<Vec<TicketOrderView>, TicketingError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let order_ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let items = sqlx::query_as::<_, BatchOrderItemRow>(
        r#"
        SELECT
            item.ticket_order_id,
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
        WHERE item.workspace_id = $1 AND item.ticket_order_id = ANY($2)
        ORDER BY ticket_type.sort_order, item.id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&order_ids)
    .fetch_all(pool)
    .await
    .map_err(TicketingError::sqlx)?;
    let tickets = sqlx::query_as::<_, BatchIssuedTicketRow>(
        r#"
        SELECT
            item.ticket_order_id,
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
        WHERE item.workspace_id = $1 AND item.ticket_order_id = ANY($2)
        ORDER BY item.id, pass.ticket_sequence
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&order_ids)
    .fetch_all(pool)
    .await
    .map_err(TicketingError::sqlx)?;
    // Query ORDER BY keeps each bucket's internal ordering identical to the
    // per-order loader; grouping appends in that same order.
    let mut grouped: HashMap<Uuid, (Vec<OrderItemRow>, Vec<IssuedTicketRow>)> =
        HashMap::with_capacity(rows.len());
    for batch in items {
        grouped.entry(batch.ticket_order_id).or_default().0.push(batch.item);
    }
    for batch in tickets {
        grouped.entry(batch.ticket_order_id).or_default().1.push(batch.ticket);
    }
    Ok(rows
        .into_iter()
        .map(|row| {
            let (items, tickets) =
                grouped.remove(&row.id).unwrap_or_else(|| (Vec::new(), Vec::new()));
            order_view(row, items, tickets)
        })
        .collect())
}

