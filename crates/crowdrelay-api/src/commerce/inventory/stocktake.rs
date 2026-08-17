async fn load_inventory_overview(
    state: &crate::AppState,
) -> Result<InventoryOverviewView, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let items = sqlx::query_as::<_, InventoryOverviewItemView>(
        r#"
        WITH stock AS (
            SELECT
                variant_id,
                COALESCE(SUM(delta), 0)::bigint AS on_hand,
                COALESCE(SUM(-delta) FILTER (
                    WHERE movement_kind = 'sale' AND delta < 0
                ), 0)::bigint AS sold_total,
                COALESCE(SUM(-delta) FILTER (
                    WHERE movement_kind = 'sale' AND delta < 0
                      AND occurred_at >= now() - interval '30 days'
                ), 0)::bigint AS sold_30d,
                COALESCE(SUM(-delta) FILTER (
                    WHERE movement_kind = 'promotional_issue' AND delta < 0
                ), 0)::bigint AS promotional_issued_total
            FROM inventory_ledger
            WHERE workspace_id = $1
            GROUP BY variant_id
        ), reservations AS (
            SELECT
                item.variant_id,
                COALESCE(SUM(item.quantity) FILTER (
                    WHERE reservation.reservation_kind = 'order'
                ), 0)::bigint AS order_reserved,
                COALESCE(SUM(item.quantity) FILTER (
                    WHERE reservation.reservation_kind = 'campaign'
                ), 0)::bigint AS campaign_reserved,
                COALESCE(SUM(item.quantity) FILTER (
                    WHERE reservation.reservation_kind = 'operational'
                ), 0)::bigint AS operational_reserved,
                COALESCE(SUM(item.quantity), 0)::bigint AS reserved,
                COUNT(DISTINCT reservation.id) FILTER (
                    WHERE reservation.reservation_kind = 'campaign'
                )::bigint AS active_campaigns
            FROM inventory_reservation_items AS item
            JOIN inventory_reservations AS reservation
              ON reservation.workspace_id = item.workspace_id
             AND reservation.id = item.reservation_id
            WHERE item.workspace_id = $1
              AND reservation.status = 'active'
              AND (reservation.expires_at IS NULL OR reservation.expires_at > now())
            GROUP BY item.variant_id
        ), counted AS (
            SELECT variant_id, MAX(created_at) AS last_counted_at
            FROM inventory_stocktake_items
            WHERE workspace_id = $1
            GROUP BY variant_id
        )
        SELECT
            product.slug AS product_slug,
            product.name AS product_name,
            variant.id AS variant_id,
            variant.sku,
            variant.label AS variant_label,
            variant.attributes,
            variant.active,
            variant.low_stock_threshold,
            variant.sell_without_stock,
            counted.variant_id IS NOT NULL AS counted,
            counted.last_counted_at,
            COALESCE(stock.on_hand, 0)::bigint AS on_hand,
            COALESCE(reservations.order_reserved, 0)::bigint AS order_reserved,
            COALESCE(reservations.campaign_reserved, 0)::bigint AS campaign_reserved,
            COALESCE(reservations.operational_reserved, 0)::bigint AS operational_reserved,
            COALESCE(reservations.reserved, 0)::bigint AS reserved,
            (COALESCE(stock.on_hand, 0) - COALESCE(reservations.reserved, 0))::bigint AS available_quantity,
            COALESCE(stock.sold_total, 0)::bigint AS sold_total,
            COALESCE(stock.sold_30d, 0)::bigint AS sold_30d,
            COALESCE(stock.promotional_issued_total, 0)::bigint AS promotional_issued_total,
            COALESCE(reservations.active_campaigns, 0)::bigint AS active_campaigns
        FROM merch_products AS product
        JOIN merch_variants AS variant
          ON variant.workspace_id = product.workspace_id
         AND variant.product_id = product.id
        LEFT JOIN stock ON stock.variant_id = variant.id
        LEFT JOIN reservations ON reservations.variant_id = variant.id
        LEFT JOIN counted ON counted.variant_id = variant.id
        WHERE product.workspace_id = $1
        ORDER BY product.slug, variant.label, variant.sku, variant.id
        "#,
    )
    .bind(workspace_id)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)?;
    Ok(InventoryOverviewView {
        generated_at: OffsetDateTime::now_utc(),
        items,
    })
}

async fn inventory_stocktake_inner(
    state: &crate::AppState,
    mutation_key: String,
    payload: InventoryStocktakeRequest,
) -> Result<InventoryStocktakeView, CommerceError> {
    if inventory_ready(state).await? {
        require_inventory_writes(state).await?;
    }
    let normalized = normalize_stocktake(payload)?;
    let request_hash = stocktake_request_hash(&normalized)?;
    let actor_id = optional_text(normalized.actor_id.as_deref(), 200)?;
    let reason = optional_text(normalized.reason.as_deref(), 500)?;
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;

    sqlx::query(
        "INSERT INTO inventory_activation_state (workspace_id) VALUES ($1) ON CONFLICT (workspace_id) DO NOTHING",
    )
    .bind(workspace_id)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    if let Some(existing) = sqlx::query_as::<_, ExistingStocktake>(
        r#"
        SELECT id, request_hash, created_at
        FROM inventory_stocktakes
        WHERE workspace_id = $1 AND idempotency_key = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(&mutation_key)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?
    {
        if existing.request_hash != request_hash {
            return Err(CommerceError::Conflict);
        }
        let items = load_stocktake_items_tx(&mut transaction, workspace_id, existing.id).await?;
        transaction.commit().await.map_err(CommerceError::sqlx)?;
        return Ok(InventoryStocktakeView {
            id: existing.id,
            replayed: true,
            created_at: existing.created_at,
            items,
        });
    }

    let stocktake_id = Uuid::now_v7();
    let created_at = sqlx::query_scalar::<_, OffsetDateTime>(
        r#"
        INSERT INTO inventory_stocktakes (
            id, workspace_id, idempotency_key, request_hash, actor_id, reason
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING created_at
        "#,
    )
    .bind(stocktake_id)
    .bind(workspace_id)
    .bind(&mutation_key)
    .bind(&request_hash)
    .bind(actor_id.as_deref())
    .bind(reason.as_deref())
    .fetch_one(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    for item in &normalized.items {
        let availability =
            lock_variant_availability(&mut transaction, workspace_id, &item.sku).await?;
        if !availability.sell_without_stock && i64::from(item.on_hand) < availability.reserved {
            return Err(CommerceError::Conflict);
        }
        let delta_i64 = i64::from(item.on_hand).saturating_sub(availability.on_hand);
        let delta = i32::try_from(delta_i64).map_err(|_| CommerceError::Invalid)?;
        if delta != 0 {
            sqlx::query(
                r#"
                INSERT INTO inventory_ledger (
                    workspace_id, variant_id, delta, movement_kind, idempotency_key,
                    actor_kind, actor_id, reason
                )
                VALUES ($1, $2, $3, 'stocktake', $4, 'admin', $5, $6)
                "#,
            )
            .bind(workspace_id)
            .bind(availability.id)
            .bind(delta)
            .bind(format!("stocktake:{stocktake_id}:{}", item.sku))
            .bind(actor_id.as_deref())
            .bind(reason.as_deref().unwrap_or("exact physical stocktake"))
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
        }
        sqlx::query(
            r#"
            INSERT INTO inventory_stocktake_items (
                workspace_id, stocktake_id, variant_id, target_on_hand,
                on_hand_before, reserved_at_apply, applied_delta
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(workspace_id)
        .bind(stocktake_id)
        .bind(availability.id)
        .bind(item.on_hand)
        .bind(availability.on_hand)
        .bind(availability.reserved)
        .bind(delta)
        .execute(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;
    }

    let items = load_stocktake_items_tx(&mut transaction, workspace_id, stocktake_id).await?;
    transaction.commit().await.map_err(CommerceError::sqlx)?;
    Ok(InventoryStocktakeView {
        id: stocktake_id,
        replayed: false,
        created_at,
        items,
    })
}

async fn load_stocktake_items_tx(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    stocktake_id: Uuid,
) -> Result<Vec<InventoryStocktakeItemView>, CommerceError> {
    sqlx::query_as::<_, InventoryStocktakeItemView>(
        r#"
        SELECT
            variant.sku,
            variant.label,
            item.target_on_hand,
            item.on_hand_before,
            item.reserved_at_apply,
            item.applied_delta,
            (item.target_on_hand::bigint - item.reserved_at_apply)::bigint AS available_quantity
        FROM inventory_stocktake_items AS item
        JOIN merch_variants AS variant
          ON variant.workspace_id = item.workspace_id
         AND variant.id = item.variant_id
        WHERE item.workspace_id = $1 AND item.stocktake_id = $2
        ORDER BY variant.sku
        "#,
    )
    .bind(workspace_id)
    .bind(stocktake_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(CommerceError::sqlx)
}

async fn mark_inventory_ready_inner(
    state: &crate::AppState,
    payload: MarkInventoryReadyRequest,
    request_id_value: Option<&str>,
) -> Result<InventoryActivationView, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let actor_id = optional_text(payload.actor_id.as_deref(), 200)?
        .unwrap_or_else(|| "virya-staff".to_owned());
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;

    sqlx::query(
        "INSERT INTO inventory_activation_state (workspace_id) VALUES ($1) ON CONFLICT (workspace_id) DO NOTHING",
    )
    .bind(workspace_id)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    let status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM inventory_activation_state WHERE workspace_id = $1 FOR UPDATE",
    )
    .bind(workspace_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    if status != "ready" {
        let _: Vec<Uuid> = sqlx::query_scalar(
            r#"
            SELECT variant.id
            FROM merch_variants AS variant
            JOIN merch_products AS product
              ON product.workspace_id = variant.workspace_id
             AND product.id = variant.product_id
            WHERE variant.workspace_id = $1 AND variant.active AND product.active
            ORDER BY variant.id
            FOR UPDATE OF variant
            "#,
        )
        .bind(workspace_id)
        .fetch_all(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;

        let missing_count = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)::bigint
            FROM merch_variants AS variant
            JOIN merch_products AS product
              ON product.workspace_id = variant.workspace_id
             AND product.id = variant.product_id
            WHERE variant.workspace_id = $1
              AND variant.active AND product.active
              AND NOT EXISTS (
                  SELECT 1 FROM inventory_stocktake_items AS item
                  WHERE item.workspace_id = variant.workspace_id
                    AND item.variant_id = variant.id
              )
            "#,
        )
        .bind(workspace_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;

        let active_count = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)::bigint
            FROM merch_variants AS variant
            JOIN merch_products AS product
              ON product.workspace_id = variant.workspace_id
             AND product.id = variant.product_id
            WHERE variant.workspace_id = $1 AND variant.active AND product.active
            "#,
        )
        .bind(workspace_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;

        let invalid_availability = sqlx::query_scalar::<_, i64>(
            r#"
            WITH stock AS (
                SELECT variant_id, COALESCE(SUM(delta), 0)::bigint AS on_hand
                FROM inventory_ledger WHERE workspace_id = $1 GROUP BY variant_id
            ), reservations AS (
                SELECT item.variant_id, COALESCE(SUM(item.quantity), 0)::bigint AS reserved
                FROM inventory_reservation_items AS item
                JOIN inventory_reservations AS reservation
                  ON reservation.workspace_id = item.workspace_id
                 AND reservation.id = item.reservation_id
                WHERE item.workspace_id = $1
                  AND reservation.status = 'active'
                  AND (reservation.expires_at IS NULL OR reservation.expires_at > now())
                GROUP BY item.variant_id
            )
            SELECT COUNT(*)::bigint
            FROM merch_variants AS variant
            JOIN merch_products AS product
              ON product.workspace_id = variant.workspace_id
             AND product.id = variant.product_id
            LEFT JOIN stock ON stock.variant_id = variant.id
            LEFT JOIN reservations ON reservations.variant_id = variant.id
            WHERE variant.workspace_id = $1
              AND variant.active AND product.active
              AND NOT variant.sell_without_stock
              AND COALESCE(stock.on_hand, 0) < COALESCE(reservations.reserved, 0)
            "#,
        )
        .bind(workspace_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;

        if active_count == 0 || missing_count > 0 || invalid_availability > 0 {
            return Err(CommerceError::Conflict);
        }

        sqlx::query(
            r#"
            UPDATE inventory_activation_state
            SET status = 'ready', ready_at = now(), ready_by = $2, version = version + 1
            WHERE workspace_id = $1
            "#,
        )
        .bind(workspace_id)
        .bind(&actor_id)
        .execute(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;
    }

    sqlx::query(
        r#"
        INSERT INTO ecosystem_feature_flags (
            workspace_id, key, enabled, reason, updated_by_request_id
        )
        SELECT $1, flag.key, true, 'inventory activated from staff panel', $2
        FROM (VALUES
            ('merch_inventory_enabled'),
            ('merch_inventory_writes_enabled'),
            ('reward_campaigns_enabled')
        ) AS flag(key)
        ON CONFLICT (workspace_id, key) DO UPDATE SET
            enabled = true,
            reason = EXCLUDED.reason,
            version = ecosystem_feature_flags.version + 1,
            updated_at = now(),
            updated_by_request_id = EXCLUDED.updated_by_request_id
        "#,
    )
    .bind(workspace_id)
    .bind(request_id_value)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    transaction.commit().await.map_err(CommerceError::sqlx)?;
    for key in [
        "merch_inventory_enabled",
        "merch_inventory_writes_enabled",
        "reward_campaigns_enabled",
    ] {
        crate::ecosystem::cache_feature_flag(workspace_id, key, true).await;
    }
    load_inventory_activation(state).await
}
