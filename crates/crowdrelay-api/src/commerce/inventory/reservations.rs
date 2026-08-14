async fn reserve_inventory_inner(
    state: &crate::AppState,
    payload: ReserveInventoryRequest,
) -> Result<InventoryReservationView, CommerceError> {
    require_inventory_writes(state).await?;
    let normalized = normalize_reservation(payload)?;
    let request_hash = reservation_request_hash(&normalized)?;
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;
    configure_transaction(&mut transaction, &state.ticketing).await?;
    expire_due_reservations(&mut transaction, workspace_id).await?;

    if let Some(existing) = sqlx::query_as::<_, ReservationRow>(
        r#"
        SELECT id, external_reference, request_hash, status, expires_at
        FROM inventory_reservations
        WHERE workspace_id = $1
          AND reservation_kind = 'order'
          AND external_reference = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(&normalized.external_reference)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?
    {
        if existing.request_hash != request_hash || existing.status != "active" {
            return Err(CommerceError::Conflict);
        }
        let view = load_reservation_view_tx(&mut transaction, workspace_id, existing.id).await?;
        transaction.commit().await.map_err(CommerceError::sqlx)?;
        return Ok(view);
    }

    let mut locked = Vec::with_capacity(normalized.items.len());
    for item in &normalized.items {
        let row = lock_variant_availability(&mut transaction, workspace_id, &item.sku).await?;
        let available = row.on_hand.saturating_sub(row.reserved);
        if !row.sell_without_stock && available < i64::from(item.quantity) {
            return Err(CommerceError::Conflict);
        }
        locked.push((row.id, item));
    }

    let reservation_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO inventory_reservations (
            id, workspace_id, reservation_kind, external_reference,
            request_hash, status, expires_at
        )
        VALUES ($1, $2, 'order', $3, $4, 'active', $5)
        "#,
    )
    .bind(reservation_id)
    .bind(workspace_id)
    .bind(&normalized.external_reference)
    .bind(request_hash)
    .bind(normalized.expires_at)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    for (variant_id, item) in locked {
        sqlx::query(
            r#"
            INSERT INTO inventory_reservation_items (
                workspace_id, reservation_id, variant_id, quantity
            )
            VALUES ($1, $2, $3, $4)
            "#,
        )
        .bind(workspace_id)
        .bind(reservation_id)
        .bind(variant_id)
        .bind(item.quantity)
        .execute(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;
    }

    let view = load_reservation_view_tx(&mut transaction, workspace_id, reservation_id).await?;
    transaction.commit().await.map_err(CommerceError::sqlx)?;
    Ok(view)
}

async fn commit_inventory_inner(
    state: &crate::AppState,
    reservation_id: Uuid,
) -> Result<InventoryReservationView, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;
    configure_transaction(&mut transaction, &state.ticketing).await?;

    let row = sqlx::query_as::<_, ReservationRow>(
        r#"
        SELECT id, external_reference, request_hash, status, expires_at
        FROM inventory_reservations
        WHERE workspace_id = $1 AND id = $2 AND reservation_kind = 'order'
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(reservation_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?
    .ok_or(CommerceError::NotFound)?;

    match row.status.as_str() {
        "committed" => {
            let view =
                load_reservation_view_tx(&mut transaction, workspace_id, reservation_id).await?;
            transaction.commit().await.map_err(CommerceError::sqlx)?;
            return Ok(view);
        }
        // Stripe payment is authoritative even when delivery of its signed
        // webhook was delayed beyond checkout expiry or arrived after an
        // out-of-order expiration/failure event released the reservation.
        // Committing an expired or released reservation can expose a temporary
        // negative stock correction, but it never loses a paid order and the
        // ledger idempotency key still prevents a double decrement.
        "active" | "expired" | "released" => {}
        _ => return Err(CommerceError::Conflict),
    }

    let items = reservation_items_tx(&mut transaction, workspace_id, reservation_id).await?;
    for item in &items {
        sqlx::query(
            r#"
            INSERT INTO inventory_ledger (
                workspace_id, variant_id, delta, movement_kind, idempotency_key,
                reservation_id, actor_kind, actor_id, reason
            )
            SELECT $1, variant.id, -$2, 'sale', $3, $4, 'stripe',
                   'stripe-checkout', 'paid Stripe checkout'
            FROM merch_variants AS variant
            WHERE variant.workspace_id = $1 AND variant.sku = $5
            ON CONFLICT (workspace_id, idempotency_key) DO NOTHING
            "#,
        )
        .bind(workspace_id)
        .bind(item.quantity)
        .bind(format!("reservation:{reservation_id}:{}", item.sku))
        .bind(reservation_id)
        .bind(&item.sku)
        .execute(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;
    }

    sqlx::query(
        r#"
        UPDATE inventory_reservations
        SET status = 'committed', committed_at = now(),
            released_at = NULL, release_reason = NULL
        WHERE workspace_id = $1 AND id = $2 AND status IN ('active', 'expired', 'released')
        "#,
    )
    .bind(workspace_id)
    .bind(reservation_id)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    let view = load_reservation_view_tx(&mut transaction, workspace_id, reservation_id).await?;
    transaction.commit().await.map_err(CommerceError::sqlx)?;
    Ok(view)
}

async fn release_inventory_inner(
    state: &crate::AppState,
    reservation_id: Uuid,
    reason: String,
) -> Result<InventoryReservationView, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let reason = clean_text(&reason, 240)?;
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;
    configure_transaction(&mut transaction, &state.ticketing).await?;

    let status: Option<String> = sqlx::query_scalar(
        r#"
        SELECT status
        FROM inventory_reservations
        WHERE workspace_id = $1 AND id = $2 AND reservation_kind = 'order'
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(reservation_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;
    let status = status.ok_or(CommerceError::NotFound)?;
    match status.as_str() {
        "active" => {
            sqlx::query(
                r#"
                UPDATE inventory_reservations
                SET status = 'released', released_at = now(), release_reason = $3
                WHERE workspace_id = $1 AND id = $2
                "#,
            )
            .bind(workspace_id)
            .bind(reservation_id)
            .bind(reason)
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
        }
        "released" | "expired" => {}
        "committed" => return Err(CommerceError::Conflict),
        _ => return Err(CommerceError::Unexpected),
    }

    let view = load_reservation_view_tx(&mut transaction, workspace_id, reservation_id).await?;
    transaction.commit().await.map_err(CommerceError::sqlx)?;
    Ok(view)
}
