use crowdrelay_application::{
    CommerceInventoryError, CommerceInventoryRepository, MarkInventoryReadyCommand,
    StocktakeCommand, StocktakeItemInput,
};

fn map_inventory_error(error: CommerceInventoryError) -> CommerceError {
    match error {
        CommerceInventoryError::NotFound => CommerceError::NotFound,
        CommerceInventoryError::Conflict => CommerceError::Conflict,
        CommerceInventoryError::Invalid => CommerceError::Invalid,
        CommerceInventoryError::Unavailable => CommerceError::Unavailable,
    }
}

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

    let command = StocktakeCommand {
        workspace_id,
        idempotency_key: mutation_key,
        request_hash,
        actor_id,
        reason,
        items: normalized
            .items
            .iter()
            .map(|item| StocktakeItemInput {
                sku: item.sku.clone(),
                on_hand: item.on_hand,
            })
            .collect(),
    };

    let result = state
        .commerce_inventory
        .stocktake(&command)
        .await
        .map_err(map_inventory_error)?;
    Ok(InventoryStocktakeView {
        id: result.id,
        replayed: result.replayed,
        created_at: result.created_at,
        items: result
            .items
            .into_iter()
            .map(|item| InventoryStocktakeItemView {
                sku: item.sku,
                label: item.label,
                target_on_hand: item.target_on_hand,
                on_hand_before: item.on_hand_before,
                reserved_at_apply: item.reserved_at_apply,
                applied_delta: item.applied_delta,
                available_quantity: item.available_quantity,
            })
            .collect(),
    })
}

async fn mark_inventory_ready_inner(
    state: &crate::AppState,
    payload: MarkInventoryReadyRequest,
    request_id_value: Option<&str>,
) -> Result<InventoryActivationView, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let actor_id = optional_text(payload.actor_id.as_deref(), 200)?
        .unwrap_or_else(|| "virya-staff".to_owned());

    let command = MarkInventoryReadyCommand {
        workspace_id,
        actor_id,
        request_id: request_id_value.map(|value| value.to_owned()),
    };

    let result = state
        .commerce_inventory
        .mark_inventory_ready(&command)
        .await
        .map_err(map_inventory_error)?;

    for key in &result.enabled_feature_flags {
        let static_key: Option<&'static str> = match key.as_str() {
            "merch_inventory_enabled" => Some("merch_inventory_enabled"),
            "merch_inventory_writes_enabled" => Some("merch_inventory_writes_enabled"),
            "reward_campaigns_enabled" => Some("reward_campaigns_enabled"),
            _ => None,
        };
        if let Some(static_key) = static_key {
            crate::ecosystem::cache_feature_flag(workspace_id, static_key, true).await;
        }
    }
    load_inventory_activation(state).await
}
