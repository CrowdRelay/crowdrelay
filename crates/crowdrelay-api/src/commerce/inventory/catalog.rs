#[derive(Debug, FromRow)]
struct ExistingLedgerMutation {
    variant_id: Uuid,
    delta: i32,
    movement_kind: String,
}

#[derive(Debug, FromRow)]
struct FulfillmentMutationRow {
    id: Uuid,
    reward_grant_id: Uuid,
    variant_id: Uuid,
    reservation_id: Uuid,
    quantity: i32,
    status: String,
}

async fn require_inventory_writes(state: &crate::AppState) -> Result<(), CommerceError> {
    if matches!(
        crate::ecosystem::feature_enabled(state, "merch_inventory_writes_enabled").await,
        Ok(true)
    ) && matches!(inventory_ready(state).await, Ok(true))
    {
        Ok(())
    } else {
        Err(CommerceError::Unavailable)
    }
}

async fn load_catalog(
    state: &crate::AppState,
    public_only: bool,
) -> Result<MerchCatalogView, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let rows = sqlx::query_as::<_, CatalogRow>(
        r#"
        WITH stock AS (
            SELECT variant_id, COALESCE(SUM(delta), 0)::bigint AS on_hand
            FROM inventory_ledger
            WHERE workspace_id = $1
            GROUP BY variant_id
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
        SELECT
            product.id AS product_id,
            product.slug AS product_slug,
            product.name AS product_name,
            product.description AS product_description,
            product.image_url AS product_image_url,
            product.currency::text AS currency,
            product.price_gross_minor,
            product.active AS product_active,
            product.public AS product_public,
            variant.id AS variant_id,
            variant.sku,
            variant.label AS variant_label,
            variant.attributes,
            variant.active AS variant_active,
            variant.low_stock_threshold,
            variant.sell_without_stock,
            COALESCE(stock.on_hand, 0)::bigint AS on_hand,
            COALESCE(reservations.reserved, 0)::bigint AS reserved
        FROM merch_products AS product
        JOIN merch_variants AS variant
          ON variant.workspace_id = product.workspace_id
         AND variant.product_id = product.id
        LEFT JOIN stock ON stock.variant_id = variant.id
        LEFT JOIN reservations ON reservations.variant_id = variant.id
        WHERE product.workspace_id = $1
          AND (
              NOT $2::boolean
              OR (product.active AND product.public AND variant.active)
          )
        ORDER BY product.slug, product.id, variant.label, variant.sku, variant.id
        "#,
    )
    .bind(workspace_id)
    .bind(public_only)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)?;

    let mut products: Vec<MerchProductView> = Vec::new();
    for row in rows {
        let available_quantity = row.on_hand.saturating_sub(row.reserved);
        let availability = if row.sell_without_stock && available_quantity <= 0 {
            "preorder"
        } else if available_quantity <= 0 {
            "out_of_stock"
        } else if available_quantity <= i64::from(row.low_stock_threshold) {
            "low_stock"
        } else {
            "in_stock"
        };
        let variant = MerchVariantView {
            id: row.variant_id,
            sku: row.sku,
            label: row.variant_label,
            attributes: row.attributes,
            active: row.variant_active,
            low_stock_threshold: row.low_stock_threshold,
            sell_without_stock: row.sell_without_stock,
            available: row.sell_without_stock || available_quantity > 0,
            on_hand: (!public_only).then_some(row.on_hand),
            reserved: (!public_only).then_some(row.reserved),
            available_quantity: (!public_only).then_some(available_quantity),
            availability,
        };

        if let Some(product) = products.last_mut()
            && product.id == row.product_id
        {
            product.variants.push(variant);
            continue;
        }
        products.push(MerchProductView {
            id: row.product_id,
            slug: row.product_slug,
            name: row.product_name,
            description: row.product_description,
            image_url: row.product_image_url,
            currency: row.currency,
            price_gross_minor: row.price_gross_minor,
            active: row.product_active,
            public: row.product_public,
            variants: vec![variant],
        });
    }

    Ok(MerchCatalogView {
        generated_at: OffsetDateTime::now_utc(),
        products,
    })
}

async fn upsert_catalog_inner(
    state: &crate::AppState,
    payload: UpsertCatalogRequest,
) -> Result<MerchCatalogView, CommerceError> {
    require_inventory_writes(state).await?;
    validate_catalog(&payload)?;
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;

    // Two statements, not two per product plus one per variant. The payload
    // caps at 100 products of 50 variants, so the loop this replaces could
    // issue 5,100 round trips inside one transaction.
    //
    // Safe as a set-based upsert precisely because `validate_catalog` has
    // already refused duplicate slugs and duplicate SKUs across the whole
    // payload. A multi-row `ON CONFLICT DO UPDATE` fails with "cannot affect
    // row a second time" when one statement touches the same conflict target
    // twice, and that is the one thing the validator makes impossible.
    let mut slugs = Vec::with_capacity(payload.products.len());
    let mut names = Vec::with_capacity(payload.products.len());
    let mut descriptions = Vec::with_capacity(payload.products.len());
    let mut image_urls = Vec::with_capacity(payload.products.len());
    let mut currencies = Vec::with_capacity(payload.products.len());
    let mut prices = Vec::with_capacity(payload.products.len());
    let mut product_active = Vec::with_capacity(payload.products.len());
    let mut product_public = Vec::with_capacity(payload.products.len());
    // Variants carry their owner's slug; the product ids come back from the
    // first statement, because a slug that already existed keeps its id.
    let mut variant_slugs = Vec::new();
    let mut skus = Vec::new();
    let mut labels = Vec::new();
    let mut attributes = Vec::new();
    let mut variant_active = Vec::new();
    let mut thresholds = Vec::new();
    let mut sell_without_stock = Vec::new();

    for product in payload.products {
        let slug = normalize_slug(&product.slug)?;
        names.push(clean_text(&product.name, 200)?);
        descriptions.push(optional_text(
            product.description.as_deref(),
            MAX_TEXT_CHARS,
        )?);
        image_urls.push(validate_optional_https_url(product.image_url.as_deref())?);
        currencies.push(product.currency.trim().to_ascii_uppercase());
        prices.push(product.price_gross_minor);
        product_active.push(product.active);
        product_public.push(product.public);

        for variant in product.variants {
            variant_slugs.push(slug.clone());
            skus.push(clean_text(&variant.sku, 128)?);
            labels.push(clean_text(&variant.label, 160)?);
            attributes.push(variant.attributes);
            variant_active.push(variant.active);
            thresholds.push(variant.low_stock_threshold);
            sell_without_stock.push(variant.sell_without_stock);
        }
        slugs.push(slug);
    }

    let product_ids: Vec<(String, Uuid)> = sqlx::query_as(
        r#"
        INSERT INTO merch_products (
            workspace_id, slug, name, description, image_url,
            currency, price_gross_minor, active, public
        )
        SELECT $1, product.slug, product.name, product.description,
               product.image_url, product.currency, product.price_gross_minor,
               product.active, product.public
        FROM unnest(
            $2::text[], $3::text[], $4::text[], $5::text[], $6::text[],
            $7::bigint[], $8::boolean[], $9::boolean[]
        ) AS product(
            slug, name, description, image_url, currency,
            price_gross_minor, active, public
        )
        ON CONFLICT (workspace_id, slug) DO UPDATE SET
            name = EXCLUDED.name,
            description = EXCLUDED.description,
            image_url = EXCLUDED.image_url,
            currency = EXCLUDED.currency,
            price_gross_minor = EXCLUDED.price_gross_minor,
            active = EXCLUDED.active,
            public = EXCLUDED.public,
            updated_at = now()
        RETURNING slug, id
        "#,
    )
    .bind(workspace_id)
    .bind(&slugs)
    .bind(&names)
    .bind(&descriptions)
    .bind(&image_urls)
    .bind(&currencies)
    .bind(&prices)
    .bind(&product_active)
    .bind(&product_public)
    .fetch_all(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    let id_of: std::collections::HashMap<&str, Uuid> = product_ids
        .iter()
        .map(|(slug, id)| (slug.as_str(), *id))
        .collect();
    // Every product was just upserted, so every slug resolves. A miss would
    // mean the statement above returned fewer rows than it wrote, which is
    // not a case to paper over with a default id.
    let mut variant_products = Vec::with_capacity(variant_slugs.len());
    for slug in &variant_slugs {
        let Some(id) = id_of.get(slug.as_str()).copied() else {
            return Err(CommerceError::Unexpected);
        };
        variant_products.push(id);
    }

    if !skus.is_empty() {
        sqlx::query(
            r#"
            INSERT INTO merch_variants (
                workspace_id, product_id, sku, label, attributes,
                active, low_stock_threshold, sell_without_stock
            )
            SELECT $1, variant.product_id, variant.sku, variant.label,
                   variant.attributes, variant.active,
                   variant.low_stock_threshold, variant.sell_without_stock
            FROM unnest(
                $2::uuid[], $3::text[], $4::text[], $5::jsonb[],
                $6::boolean[], $7::int4[], $8::boolean[]
            ) AS variant(
                product_id, sku, label, attributes, active,
                low_stock_threshold, sell_without_stock
            )
            ON CONFLICT (workspace_id, sku) DO UPDATE SET
                product_id = EXCLUDED.product_id,
                label = EXCLUDED.label,
                attributes = EXCLUDED.attributes,
                active = EXCLUDED.active,
                low_stock_threshold = EXCLUDED.low_stock_threshold,
                sell_without_stock = EXCLUDED.sell_without_stock,
                updated_at = now()
            "#,
        )
        .bind(workspace_id)
        .bind(&variant_products)
        .bind(&skus)
        .bind(&labels)
        .bind(&attributes)
        .bind(&variant_active)
        .bind(&thresholds)
        .bind(&sell_without_stock)
        .execute(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;
    }

    transaction.commit().await.map_err(CommerceError::sqlx)?;
    load_catalog(state, false).await
}

async fn adjust_inventory_inner(
    state: &crate::AppState,
    mutation_key: String,
    payload: AdjustInventoryRequest,
) -> Result<InventoryAdjustmentView, CommerceError> {
    require_inventory_writes(state).await?;
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let sku = clean_text(&payload.sku, 128)?;
    let movement_kind = clean_movement_kind(&payload.movement_kind)?;
    if payload.delta == 0 {
        return Err(CommerceError::Invalid);
    }
    let actor_id = optional_text(payload.actor_id.as_deref(), 200)?;
    let reason = optional_text(payload.reason.as_deref(), 500)?;

    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;

    let availability = lock_variant_availability(&mut transaction, workspace_id, &sku).await?;
    if let Some(existing) = sqlx::query_as::<_, ExistingLedgerMutation>(
        r#"
        SELECT variant_id, delta, movement_kind
        FROM inventory_ledger
        WHERE workspace_id = $1 AND idempotency_key = $2
        "#,
    )
    .bind(workspace_id)
    .bind(&mutation_key)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?
    {
        if existing.variant_id != availability.id
            || existing.delta != payload.delta
            || existing.movement_kind != movement_kind
        {
            return Err(CommerceError::Conflict);
        }
        transaction.commit().await.map_err(CommerceError::sqlx)?;
        return inventory_adjustment_view(state, &sku, payload.delta, &movement_kind).await;
    }

    let projected_on_hand = availability
        .on_hand
        .saturating_add(i64::from(payload.delta));
    if payload.delta < 0
        && !availability.sell_without_stock
        && projected_on_hand < availability.reserved
    {
        return Err(CommerceError::Conflict);
    }

    sqlx::query(
        r#"
        INSERT INTO inventory_ledger (
            workspace_id, variant_id, delta, movement_kind, idempotency_key,
            actor_kind, actor_id, reason
        )
        VALUES ($1, $2, $3, $4, $5, 'admin', $6, $7)
        "#,
    )
    .bind(workspace_id)
    .bind(availability.id)
    .bind(payload.delta)
    .bind(&movement_kind)
    .bind(&mutation_key)
    .bind(actor_id)
    .bind(reason)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    transaction.commit().await.map_err(CommerceError::sqlx)?;
    inventory_adjustment_view(state, &sku, payload.delta, &movement_kind).await
}

async fn inventory_adjustment_view(
    state: &crate::AppState,
    sku: &str,
    delta: i32,
    movement_kind: &str,
) -> Result<InventoryAdjustmentView, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let row = variant_availability(state.ticketing.pool(), workspace_id, sku).await?;
    Ok(InventoryAdjustmentView {
        sku: row.sku,
        delta,
        movement_kind: movement_kind.to_owned(),
        on_hand: row.on_hand,
        reserved: row.reserved,
        available_quantity: row.on_hand.saturating_sub(row.reserved),
    })
}

async fn ensure_inventory_activation_row(state: &crate::AppState) -> Result<(), CommerceError> {
    sqlx::query(
        r#"
        INSERT INTO inventory_activation_state (workspace_id)
        VALUES ($1)
        ON CONFLICT (workspace_id) DO NOTHING
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .execute(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)?;
    Ok(())
}

async fn inventory_ready(state: &crate::AppState) -> Result<bool, CommerceError> {
    ensure_inventory_activation_row(state).await?;
    sqlx::query_scalar::<_, bool>(
        "SELECT status = 'ready' FROM inventory_activation_state WHERE workspace_id = $1",
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_one(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)
}

async fn load_inventory_activation(
    state: &crate::AppState,
) -> Result<InventoryActivationView, CommerceError> {
    ensure_inventory_activation_row(state).await?;
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let row = sqlx::query_as::<_, InventoryActivationRow>(
        r#"
        SELECT status, catalog_seed_version, catalog_seeded_at,
               ready_at, ready_by, version
        FROM inventory_activation_state
        WHERE workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .fetch_one(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)?;

    let total_active_variants = sqlx::query_scalar::<_, i64>(
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
    .fetch_one(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)?;

    let missing_skus = sqlx::query_scalar::<_, String>(
        r#"
        SELECT variant.sku
        FROM merch_variants AS variant
        JOIN merch_products AS product
          ON product.workspace_id = variant.workspace_id
         AND product.id = variant.product_id
        WHERE variant.workspace_id = $1
          AND variant.active
          AND product.active
          AND NOT EXISTS (
              SELECT 1
              FROM inventory_stocktake_items AS item
              WHERE item.workspace_id = variant.workspace_id
                AND item.variant_id = variant.id
          )
        ORDER BY product.slug, variant.label, variant.sku
        "#,
    )
    .bind(workspace_id)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)?;
    let counted_active_variants =
        total_active_variants.saturating_sub(i64::try_from(missing_skus.len()).unwrap_or(i64::MAX));

    let invalid_availability = sqlx::query_scalar::<_, i64>(
        r#"
        WITH stock AS (
            SELECT variant_id, COALESCE(SUM(delta), 0)::bigint AS on_hand
            FROM inventory_ledger
            WHERE workspace_id = $1
            GROUP BY variant_id
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
          AND variant.active
          AND product.active
          AND NOT variant.sell_without_stock
          AND COALESCE(stock.on_hand, 0) < COALESCE(reservations.reserved, 0)
        "#,
    )
    .bind(workspace_id)
    .fetch_one(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)?;

    let flags = sqlx::query_as::<_, (String, bool)>(
        r#"
        SELECT key, enabled
        FROM ecosystem_feature_flags
        WHERE workspace_id = $1
          AND key IN (
              'merch_inventory_enabled',
              'merch_inventory_writes_enabled',
              'reward_campaigns_enabled'
          )
        "#,
    )
    .bind(workspace_id)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)?;
    let flag = |key: &str| {
        flags
            .iter()
            .any(|(candidate, enabled)| candidate == key && *enabled)
    };
    let public_enabled = flag("merch_inventory_enabled");
    let writes_enabled = flag("merch_inventory_writes_enabled");
    let campaigns_enabled = flag("reward_campaigns_enabled");
    let fully_enabled = public_enabled && writes_enabled && campaigns_enabled;

    let mut blockers = Vec::new();
    if total_active_variants == 0 || row.catalog_seeded_at.is_none() {
        blockers.push("catalog_empty".to_owned());
    }
    if !missing_skus.is_empty() {
        blockers.push("uncounted_variants".to_owned());
    }
    if invalid_availability > 0 {
        blockers.push("reserved_exceeds_stock".to_owned());
    }
    let ready = row.status == "ready";
    if ready && !fully_enabled {
        blockers.push("feature_flags_inconsistent".to_owned());
    }
    let can_mark_ready = blockers
        .iter()
        .all(|blocker| blocker == "feature_flags_inconsistent");

    Ok(InventoryActivationView {
        status: row.status,
        ready,
        fully_enabled,
        catalog_seed_version: row.catalog_seed_version,
        catalog_seeded_at: row.catalog_seeded_at,
        ready_at: row.ready_at,
        ready_by: row.ready_by,
        version: row.version,
        total_active_variants,
        counted_active_variants,
        missing_skus,
        blockers,
        can_mark_ready,
        public_enabled,
        writes_enabled,
        campaigns_enabled,
    })
}
