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
