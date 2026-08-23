// The concrete state changes an executed action makes.
//
// Split from the execution chunk so that one stays about deciding what to do
// with an action, and this one stays about the guarded updates themselves —
// each of which checks the version it was authorised against before writing.
// Spliced with `include!`, so it carries no module header of its own.

async fn execute_ticket_price_change(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    ticket_type_id: TicketTypeId,
    from_minor: i64,
    to_minor: i64,
) -> Result<(), RepositoryError> {
    let updated = sqlx::query(
        r#"
        UPDATE ticket_types
        SET price_gross_minor = $4
        WHERE workspace_id = $1
          AND id = $2
          AND active
          AND price_gross_minor = $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(ticket_type_id.into_uuid())
    .bind(from_minor)
    .bind(to_minor)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if updated.rows_affected() == 1 {
        return Ok(());
    }
    let current = sqlx::query_scalar::<_, i64>(
        "SELECT price_gross_minor FROM ticket_types WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(ticket_type_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    match current {
        Some(current) if current == to_minor => Ok(()),
        Some(_) => Err(RepositoryError::Conflict),
        None => Err(RepositoryError::NotFound),
    }
}

async fn execute_ticket_capacity_change(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    ticket_type_id: TicketTypeId,
    from_capacity: u32,
    to_capacity: u32,
    expected_guardrail_version: i64,
) -> Result<(), RepositoryError> {
    if to_capacity <= from_capacity {
        return Err(RepositoryError::Conflict);
    }
    let row = sqlx::query_as::<_, (Option<i32>, i32, i32, i32, i64)>(
        r#"
        SELECT ticket_type.capacity, ticket_sale.capacity,
               guardrail.minimum_capacity, guardrail.maximum_capacity, guardrail.version
        FROM ticket_types AS ticket_type
        JOIN ticket_sales AS ticket_sale
          ON ticket_sale.workspace_id = ticket_type.workspace_id
         AND ticket_sale.id = ticket_type.ticket_sale_id
        JOIN viryaos_ticket_type_allocation_guardrails AS guardrail
          ON guardrail.workspace_id = ticket_type.workspace_id
         AND guardrail.ticket_type_id = ticket_type.id
        WHERE ticket_type.workspace_id = $1
          AND ticket_type.id = $2
          AND ticket_type.active
          AND ticket_sale.active
        FOR UPDATE OF ticket_type, guardrail
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(ticket_type_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;
    let current_capacity = row.0.ok_or(RepositoryError::Conflict)?;
    let from_i32 = i32::try_from(from_capacity).map_err(|_| RepositoryError::Unexpected)?;
    let to_i32 = i32::try_from(to_capacity).map_err(|_| RepositoryError::Unexpected)?;
    if row.4 != expected_guardrail_version
        || current_capacity != from_i32
        || to_i32 > row.1
        || from_i32 < row.2
        || to_i32 > row.3
    {
        return Err(RepositoryError::Conflict);
    }
    let committed = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COALESCE(SUM(item.quantity), 0)::bigint
        FROM ticket_order_items AS item
        JOIN ticket_orders AS ticket_order
          ON ticket_order.workspace_id = item.workspace_id
         AND ticket_order.id = item.ticket_order_id
        WHERE item.workspace_id = $1
          AND item.ticket_type_id = $2
          AND ticket_order.status IN ('reserved', 'paid')
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(ticket_type_id.into_uuid())
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if committed > i64::from(to_i32) {
        return Err(RepositoryError::Conflict);
    }
    let updated = sqlx::query(
        r#"
        UPDATE ticket_types
        SET capacity = $4
        WHERE workspace_id = $1
          AND id = $2
          AND active
          AND capacity = $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(ticket_type_id.into_uuid())
    .bind(from_i32)
    .bind(to_i32)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if updated.rows_affected() == 1 {
        Ok(())
    } else {
        Err(RepositoryError::Conflict)
    }
}

async fn execute_merch_price_change(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    product_id: MerchProductId,
    from_minor: i64,
    to_minor: i64,
    expected_economics_version: i64,
) -> Result<(), RepositoryError> {
    let guardrails = sqlx::query_as::<_, (i64, i64, i64)>(
        r#"
        SELECT minimum_price_minor, maximum_price_minor, version
        FROM viryaos_merch_product_economics
        WHERE workspace_id = $1 AND product_id = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(product_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;
    if guardrails.2 != expected_economics_version
        || to_minor < guardrails.0
        || to_minor > guardrails.1
    {
        return Err(RepositoryError::Conflict);
    }
    let updated = sqlx::query(
        r#"
        UPDATE merch_products
        SET price_gross_minor = $4
        WHERE workspace_id = $1
          AND id = $2
          AND active
          AND public
          AND price_gross_minor = $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(product_id.into_uuid())
    .bind(from_minor)
    .bind(to_minor)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    if updated.rows_affected() == 1 {
        return Ok(());
    }
    let current = sqlx::query_scalar::<_, i64>(
        "SELECT price_gross_minor FROM merch_products WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(product_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    match current {
        Some(current) if current == to_minor => Ok(()),
        Some(_) => Err(RepositoryError::Conflict),
        None => Err(RepositoryError::NotFound),
    }
}
