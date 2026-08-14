async fn consume_campaign_reservation_item(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    reservation_id: Uuid,
    variant_id: Uuid,
    quantity: i32,
) -> Result<(), CommerceError> {
    let current: i32 = sqlx::query_scalar(
        r#"
        SELECT quantity
        FROM inventory_reservation_items
        WHERE workspace_id = $1 AND reservation_id = $2 AND variant_id = $3
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(reservation_id)
    .bind(variant_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(CommerceError::sqlx)?
    .ok_or(CommerceError::Conflict)?;
    if current < quantity {
        return Err(CommerceError::Conflict);
    }
    if current == quantity {
        sqlx::query(
            r#"
            DELETE FROM inventory_reservation_items
            WHERE workspace_id = $1 AND reservation_id = $2 AND variant_id = $3
            "#,
        )
        .bind(workspace_id)
        .bind(reservation_id)
        .bind(variant_id)
        .execute(&mut **transaction)
        .await
        .map_err(CommerceError::sqlx)?;
    } else {
        sqlx::query(
            r#"
            UPDATE inventory_reservation_items
            SET quantity = quantity - $4
            WHERE workspace_id = $1 AND reservation_id = $2 AND variant_id = $3
            "#,
        )
        .bind(workspace_id)
        .bind(reservation_id)
        .bind(variant_id)
        .bind(quantity)
        .execute(&mut **transaction)
        .await
        .map_err(CommerceError::sqlx)?;
    }
    Ok(())
}

async fn finalize_campaign_reservation_if_empty(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    reservation_id: Uuid,
) -> Result<(), CommerceError> {
    sqlx::query(
        r#"
        UPDATE inventory_reservations AS reservation
        SET status = CASE
                WHEN EXISTS (
                    SELECT 1
                    FROM inventory_ledger AS ledger
                    WHERE ledger.workspace_id = reservation.workspace_id
                      AND ledger.reservation_id = reservation.id
                ) THEN 'committed'
                ELSE 'released'
            END,
            committed_at = CASE
                WHEN EXISTS (
                    SELECT 1
                    FROM inventory_ledger AS ledger
                    WHERE ledger.workspace_id = reservation.workspace_id
                      AND ledger.reservation_id = reservation.id
                ) THEN now()
                ELSE NULL
            END,
            released_at = CASE
                WHEN EXISTS (
                    SELECT 1
                    FROM inventory_ledger AS ledger
                    WHERE ledger.workspace_id = reservation.workspace_id
                      AND ledger.reservation_id = reservation.id
                ) THEN NULL
                ELSE now()
            END,
            release_reason = CASE
                WHEN EXISTS (
                    SELECT 1
                    FROM inventory_ledger AS ledger
                    WHERE ledger.workspace_id = reservation.workspace_id
                      AND ledger.reservation_id = reservation.id
                ) THEN NULL
                ELSE 'campaign allocation closed without delivery'
            END
        WHERE reservation.workspace_id = $1
          AND reservation.id = $2
          AND reservation.status = 'active'
          AND NOT EXISTS (
              SELECT 1
              FROM inventory_reservation_items AS item
              WHERE item.workspace_id = reservation.workspace_id
                AND item.reservation_id = reservation.id
          )
        "#,
    )
    .bind(workspace_id)
    .bind(reservation_id)
    .execute(&mut **transaction)
    .await
    .map_err(CommerceError::sqlx)?;
    Ok(())
}

async fn reservation_items_tx(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    reservation_id: Uuid,
) -> Result<Vec<InventoryReservationItemView>, CommerceError> {
    sqlx::query_as::<_, InventoryReservationItemView>(
        r#"
        SELECT variant.sku, variant.label, item.quantity
        FROM inventory_reservation_items AS item
        JOIN merch_variants AS variant
          ON variant.workspace_id = item.workspace_id
         AND variant.id = item.variant_id
        WHERE item.workspace_id = $1 AND item.reservation_id = $2
        ORDER BY variant.sku, variant.id
        "#,
    )
    .bind(workspace_id)
    .bind(reservation_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(CommerceError::sqlx)
}

async fn load_reservation_view_tx(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    reservation_id: Uuid,
) -> Result<InventoryReservationView, CommerceError> {
    let row = sqlx::query_as::<_, ReservationRow>(
        r#"
        SELECT id, external_reference, request_hash, status, expires_at
        FROM inventory_reservations
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(reservation_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(CommerceError::sqlx)?
    .ok_or(CommerceError::NotFound)?;
    let items = reservation_items_tx(transaction, workspace_id, reservation_id).await?;
    Ok(InventoryReservationView {
        id: row.id,
        external_reference: row.external_reference,
        status: row.status,
        expires_at: row.expires_at,
        items,
    })
}

async fn expire_due_reservations(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
) -> Result<(), CommerceError> {
    sqlx::query(
        r#"
        UPDATE inventory_reservations
        SET status = 'expired', released_at = now(), release_reason = 'reservation expired'
        WHERE workspace_id = $1
          AND status = 'active'
          AND expires_at IS NOT NULL
          AND expires_at <= now()
        "#,
    )
    .bind(workspace_id)
    .execute(&mut **transaction)
    .await
    .map_err(CommerceError::sqlx)?;
    Ok(())
}

async fn lock_variant_availability(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    sku: &str,
) -> Result<VariantAvailabilityRow, CommerceError> {
    sqlx::query_as::<_, VariantAvailabilityRow>(
        r#"
        SELECT
            variant.id,
            product.name AS product_name,
            variant.sku,
            variant.sell_without_stock,
            COALESCE((
                SELECT SUM(ledger.delta)::bigint
                FROM inventory_ledger AS ledger
                WHERE ledger.workspace_id = variant.workspace_id
                  AND ledger.variant_id = variant.id
            ), 0)::bigint AS on_hand,
            COALESCE((
                SELECT SUM(item.quantity)::bigint
                FROM inventory_reservation_items AS item
                JOIN inventory_reservations AS reservation
                  ON reservation.workspace_id = item.workspace_id
                 AND reservation.id = item.reservation_id
                WHERE item.workspace_id = variant.workspace_id
                  AND item.variant_id = variant.id
                  AND reservation.status = 'active'
                  AND (reservation.expires_at IS NULL OR reservation.expires_at > now())
            ), 0)::bigint AS reserved
        FROM merch_variants AS variant
        JOIN merch_products AS product
          ON product.workspace_id = variant.workspace_id
         AND product.id = variant.product_id
        WHERE variant.workspace_id = $1 AND variant.sku = $2 AND variant.active
        FOR UPDATE OF variant
        "#,
    )
    .bind(workspace_id)
    .bind(sku)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(CommerceError::sqlx)?
    .ok_or(CommerceError::NotFound)
}

async fn variant_availability(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    sku: &str,
) -> Result<VariantAvailabilityRow, CommerceError> {
    sqlx::query_as::<_, VariantAvailabilityRow>(
        r#"
        SELECT
            variant.id,
            product.name AS product_name,
            variant.sku,
            variant.sell_without_stock,
            COALESCE((
                SELECT SUM(ledger.delta)::bigint
                FROM inventory_ledger AS ledger
                WHERE ledger.workspace_id = variant.workspace_id
                  AND ledger.variant_id = variant.id
            ), 0)::bigint AS on_hand,
            COALESCE((
                SELECT SUM(item.quantity)::bigint
                FROM inventory_reservation_items AS item
                JOIN inventory_reservations AS reservation
                  ON reservation.workspace_id = item.workspace_id
                 AND reservation.id = item.reservation_id
                WHERE item.workspace_id = variant.workspace_id
                  AND item.variant_id = variant.id
                  AND reservation.status = 'active'
                  AND (reservation.expires_at IS NULL OR reservation.expires_at > now())
            ), 0)::bigint AS reserved
        FROM merch_variants AS variant
        JOIN merch_products AS product
          ON product.workspace_id = variant.workspace_id
         AND product.id = variant.product_id
        WHERE variant.workspace_id = $1 AND variant.sku = $2
        "#,
    )
    .bind(workspace_id)
    .bind(sku)
    .fetch_optional(pool)
    .await
    .map_err(CommerceError::sqlx)?
    .ok_or(CommerceError::NotFound)
}
