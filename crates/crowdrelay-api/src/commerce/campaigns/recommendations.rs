async fn load_promotion_recommendations(
    state: &crate::AppState,
) -> Result<Vec<PromotionRecommendationView>, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    sqlx::query_as::<_, PromotionRecommendationView>(
        r#"
        WITH stock AS (
            SELECT
                variant.id AS variant_id,
                COALESCE(SUM(ledger.delta), 0)::bigint AS on_hand,
                MIN(ledger.occurred_at) AS first_movement_at,
                COALESCE(SUM(-ledger.delta) FILTER (
                    WHERE ledger.movement_kind = 'sale'
                      AND ledger.delta < 0
                      AND ledger.occurred_at >= now() - interval '7 days'
                ), 0)::bigint AS sold_7d,
                COALESCE(SUM(-ledger.delta) FILTER (
                    WHERE ledger.movement_kind = 'sale'
                      AND ledger.delta < 0
                      AND ledger.occurred_at >= now() - interval '30 days'
                ), 0)::bigint AS sold_30d,
                COALESCE(SUM(-ledger.delta) FILTER (
                    WHERE ledger.movement_kind = 'sale'
                      AND ledger.delta < 0
                      AND ledger.occurred_at >= now() - interval '90 days'
                ), 0)::bigint AS sold_90d,
                COALESCE(SUM(-ledger.delta) FILTER (
                    WHERE ledger.movement_kind = 'promotional_issue'
                      AND ledger.delta < 0
                      AND ledger.occurred_at >= now() - interval '90 days'
                ), 0)::bigint AS promotional_issued_90d
            FROM merch_variants AS variant
            LEFT JOIN inventory_ledger AS ledger
              ON ledger.workspace_id = variant.workspace_id
             AND ledger.variant_id = variant.id
            WHERE variant.workspace_id = $1
            GROUP BY variant.id
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
        ), event_pressure AS (
            SELECT COUNT(*)::bigint AS upcoming_events_60d
            FROM events
            WHERE workspace_id = $1
              AND status <> 'cancelled'
              AND starts_at >= now()
              AND starts_at < now() + interval '60 days'
        ), inputs AS (
            SELECT
                variant.sku,
                product.name AS product_name,
                variant.label AS variant_label,
                COALESCE(stock.on_hand, 0)::bigint AS on_hand,
                COALESCE(reservations.reserved, 0)::bigint AS reserved,
                GREATEST(
                    COALESCE(stock.on_hand, 0) - COALESCE(reservations.reserved, 0),
                    0
                )::bigint AS available_quantity,
                COALESCE(stock.sold_7d, 0)::bigint AS sold_7d,
                COALESCE(stock.sold_30d, 0)::bigint AS sold_30d,
                COALESCE(stock.sold_90d, 0)::bigint AS sold_90d,
                COALESCE(stock.promotional_issued_90d, 0)::bigint AS promotional_issued_90d,
                event_pressure.upcoming_events_60d,
                GREATEST(
                    COALESCE(EXTRACT(day FROM now() - stock.first_movement_at), 0),
                    0
                )::integer AS history_days,
                GREATEST(
                    variant.low_stock_threshold::bigint * 2,
                    COALESCE(stock.sold_30d, 0)::bigint,
                    event_pressure.upcoming_events_60d * 2
                )::bigint AS safety_stock
            FROM merch_variants AS variant
            JOIN merch_products AS product
              ON product.workspace_id = variant.workspace_id
             AND product.id = variant.product_id
            LEFT JOIN stock ON stock.variant_id = variant.id
            LEFT JOIN reservations ON reservations.variant_id = variant.id
            CROSS JOIN event_pressure
            WHERE variant.workspace_id = $1
              AND variant.active
              AND product.active
        ), scored AS (
            SELECT
                inputs.*,
                CASE
                    WHEN sold_30d >= 3
                     AND sold_30d * 2 > GREATEST(sold_90d - sold_30d, 0)
                    THEN 0
                    WHEN history_days < 30
                    THEN LEAST(
                        GREATEST(available_quantity - safety_stock, 0),
                        available_quantity / 4
                    )
                    ELSE GREATEST(available_quantity - safety_stock, 0)
                END::bigint AS recommended_max_giveaway
            FROM inputs
        )
        SELECT
            sku,
            product_name,
            variant_label,
            on_hand,
            reserved,
            available_quantity,
            sold_7d,
            sold_30d,
            sold_90d,
            promotional_issued_90d,
            upcoming_events_60d,
            history_days,
            safety_stock,
            recommended_max_giveaway,
            CASE
                WHEN recommended_max_giveaway = 0 THEN 'hold'
                WHEN recommended_max_giveaway >= 5 THEN 'candidate'
                ELSE 'limited'
            END AS recommendation,
            CASE
                WHEN history_days < 30 THEN 'low'
                WHEN history_days < 90 THEN 'medium'
                ELSE 'high'
            END AS confidence,
            CASE
                WHEN sold_30d >= 3
                 AND sold_30d * 2 > GREATEST(sold_90d - sold_30d, 0)
                THEN 'Rosnąca sprzedaż — zachowaj stock dla zamówień.'
                WHEN available_quantity <= safety_stock
                THEN 'Dostępny stan nie przekracza konserwatywnego zapasu bezpieczeństwa.'
                WHEN history_days < 30
                THEN 'Historia jest krótka — rekomendacja ograniczona do 25% nadwyżki.'
                WHEN recommended_max_giveaway >= 5
                THEN 'Jest nadwyżka ponad sprzedaż, niski stan i presję najbliższych koncertów.'
                ELSE 'Możliwa mała akcja promocyjna bez naruszania zapasu bezpieczeństwa.'
            END AS reason
        FROM scored
        ORDER BY recommended_max_giveaway DESC, product_name, variant_label, sku
        "#,
    )
    .bind(workspace_id)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)
}
