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

async fn load_reward_campaigns(
    state: &crate::AppState,
) -> Result<Vec<RewardCampaignView>, CommerceError> {
    load_reward_campaigns_filtered(state, None).await
}

async fn load_reward_campaigns_filtered(
    state: &crate::AppState,
    draw_id: Option<Uuid>,
) -> Result<Vec<RewardCampaignView>, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    sqlx::query_as::<_, RewardCampaignView>(
        r#"
        SELECT
            draw.id,
            draw.slug,
            draw.name,
            draw.status,
            draw.eligibility_kind,
            event.slug AS event_slug,
            draw.winner_count,
            COALESCE(winner_totals.selected_winners, 0)::bigint AS selected_winners,
            variant.sku AS prize_sku,
            product.name AS prize_name,
            variant.label AS prize_variant,
            allocation.units_per_winner,
            COALESCE(reservation_item.quantity, 0)::integer AS reserved_quantity,
            COALESCE(fulfillment_totals.pending_fulfillments, 0)::bigint AS pending_fulfillments,
            COALESCE(fulfillment_totals.delivered_fulfillments, 0)::bigint AS delivered_fulfillments,
            draw.opens_at,
            draw.closes_at,
            draw.draw_at,
            draw.completed_at
        FROM reward_draws AS draw
        JOIN reward_draw_inventory_allocations AS allocation
          ON allocation.workspace_id = draw.workspace_id
         AND allocation.draw_id = draw.id
        JOIN merch_variants AS variant
          ON variant.workspace_id = allocation.workspace_id
         AND variant.id = allocation.variant_id
        LEFT JOIN inventory_reservations AS allocation_reservation
          ON allocation_reservation.workspace_id = allocation.workspace_id
         AND allocation_reservation.id = allocation.reservation_id
         AND allocation_reservation.status = 'active'
        LEFT JOIN inventory_reservation_items AS reservation_item
          ON reservation_item.workspace_id = allocation_reservation.workspace_id
         AND reservation_item.reservation_id = allocation_reservation.id
         AND reservation_item.variant_id = allocation.variant_id
        JOIN merch_products AS product
          ON product.workspace_id = variant.workspace_id
         AND product.id = variant.product_id
        LEFT JOIN events AS event
          ON event.workspace_id = draw.workspace_id
         AND event.id = draw.event_id
        LEFT JOIN LATERAL (
            SELECT COUNT(*)::bigint AS selected_winners
            FROM reward_draw_winners AS winner
            WHERE winner.workspace_id = draw.workspace_id
              AND winner.draw_id = draw.id
        ) AS winner_totals ON true
        LEFT JOIN LATERAL (
            SELECT
                COUNT(*) FILTER (WHERE fulfillment.status IN ('pending', 'prepared'))::bigint
                    AS pending_fulfillments,
                COUNT(*) FILTER (WHERE fulfillment.status = 'delivered')::bigint
                    AS delivered_fulfillments
            FROM reward_draw_fulfillments AS fulfillment
            WHERE fulfillment.workspace_id = draw.workspace_id
              AND fulfillment.draw_id = draw.id
        ) AS fulfillment_totals ON true
        WHERE draw.workspace_id = $1
          AND ($2::uuid IS NULL OR draw.id = $2)
        ORDER BY draw.created_at DESC, draw.id DESC
        "#,
    )
    .bind(workspace_id)
    .bind(draw_id)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)
}

async fn create_reward_campaign_inner(
    state: &crate::AppState,
    payload: CreateRewardCampaignRequest,
) -> Result<RewardCampaignView, CommerceError> {
    require_inventory_writes(state).await?;
    validate_reward_campaign(&payload)?;
    if payload.status == "scheduled"
        && !matches!(
            crate::ecosystem::feature_enabled(state, "reward_campaigns_enabled").await,
            Ok(true)
        )
    {
        return Err(CommerceError::Conflict);
    }

    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let slug = normalize_slug(&payload.slug)?;
    let name = clean_text(&payload.name, 200)?;
    let prize_sku = clean_text(&payload.prize_sku, 128)?;
    let event_slug = payload
        .event_slug
        .as_deref()
        .map(normalize_slug)
        .transpose()?;
    let reserved_quantity = payload
        .winner_count
        .checked_mul(payload.units_per_winner)
        .ok_or(CommerceError::Invalid)?;

    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;
    configure_transaction(&mut transaction, &state.ticketing).await?;

    let already_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM reward_draws WHERE workspace_id = $1 AND slug = $2)",
    )
    .bind(workspace_id)
    .bind(&slug)
    .fetch_one(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;
    if already_exists {
        return Err(CommerceError::Conflict);
    }

    let variant = lock_variant_availability(&mut transaction, workspace_id, &prize_sku).await?;
    if !variant.sell_without_stock
        && variant.on_hand.saturating_sub(variant.reserved) < i64::from(reserved_quantity)
    {
        return Err(CommerceError::Conflict);
    }

    let event_id = match payload.eligibility_kind.as_str() {
        "all_active" => None,
        "event_interest" => {
            let slug = event_slug.as_deref().ok_or(CommerceError::Invalid)?;
            Some(
                sqlx::query_scalar::<_, Uuid>(
                    r#"
                    SELECT id
                    FROM events
                    WHERE workspace_id = $1 AND slug = $2 AND status <> 'cancelled'
                    FOR SHARE
                    "#,
                )
                .bind(workspace_id)
                .bind(slug)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(CommerceError::sqlx)?
                .ok_or(CommerceError::NotFound)?,
            )
        }
        _ => return Err(CommerceError::Invalid),
    };

    let reward_rule_id = Uuid::now_v7();
    let draw_id = Uuid::now_v7();
    let reservation_id = Uuid::now_v7();
    let expires_days = (payload.claim_expires_hours.saturating_add(23) / 24).clamp(1, 3650);
    sqlx::query(
        r#"
        INSERT INTO reward_rules (
            id, workspace_id, name, reward_type, threshold, config, active
        )
        VALUES ($1, $2, $3, 'physical_item', NULL, $4, true)
        "#,
    )
    .bind(reward_rule_id)
    .bind(workspace_id)
    .bind(format!("campaign:{slug}"))
    .bind(json!({
        "item_name": variant.product_name,
        "sku": variant.sku,
        "expires_days": expires_days,
    }))
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    sqlx::query(
        r#"
        INSERT INTO reward_draws (
            id, workspace_id, slug, name, prize_kind, eligibility_kind,
            event_id, reward_rule_id, winner_count, base_entries,
            entries_per_referral, entries_per_checkin, max_entries,
            claim_expires_hours, opens_at, closes_at, draw_at, status
        )
        VALUES (
            $1, $2, $3, $4, 'physical_item', $5,
            $6, $7, $8, $9, $10, $11, $12,
            $13, $14, $15, $16, $17
        )
        "#,
    )
    .bind(draw_id)
    .bind(workspace_id)
    .bind(&slug)
    .bind(name)
    .bind(&payload.eligibility_kind)
    .bind(event_id)
    .bind(reward_rule_id)
    .bind(payload.winner_count)
    .bind(payload.base_entries)
    .bind(payload.entries_per_referral)
    .bind(payload.entries_per_checkin)
    .bind(payload.max_entries)
    .bind(payload.claim_expires_hours)
    .bind(payload.opens_at)
    .bind(payload.closes_at)
    .bind(payload.draw_at)
    .bind(&payload.status)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    let reservation_hash = Sha256::digest(
        serde_json::to_vec(&json!({
            "draw_id": draw_id,
            "sku": prize_sku,
            "quantity": reserved_quantity,
        }))
        .map_err(|_| CommerceError::Unexpected)?,
    );
    sqlx::query(
        r#"
        INSERT INTO inventory_reservations (
            id, workspace_id, reservation_kind, external_reference,
            request_hash, status, expires_at
        )
        VALUES ($1, $2, 'campaign', $3, $4, 'active', NULL)
        "#,
    )
    .bind(reservation_id)
    .bind(workspace_id)
    .bind(format!("reward-draw:{draw_id}"))
    .bind(reservation_hash.as_slice())
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

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
    .bind(variant.id)
    .bind(reserved_quantity)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    sqlx::query(
        r#"
        INSERT INTO reward_draw_inventory_allocations (
            workspace_id, draw_id, variant_id, reservation_id,
            units_per_winner, reserved_quantity
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(workspace_id)
    .bind(draw_id)
    .bind(variant.id)
    .bind(reservation_id)
    .bind(payload.units_per_winner)
    .bind(reserved_quantity)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    transaction.commit().await.map_err(CommerceError::sqlx)?;
    load_reward_campaigns_filtered(state, Some(draw_id))
        .await?
        .into_iter()
        .next()
        .ok_or(CommerceError::Unexpected)
}

async fn schedule_reward_campaign_inner(
    state: &crate::AppState,
    draw_id: Uuid,
) -> Result<RewardCampaignView, CommerceError> {
    if !matches!(
        crate::ecosystem::feature_enabled(state, "reward_campaigns_enabled").await,
        Ok(true)
    ) {
        return Err(CommerceError::Conflict);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;
    configure_transaction(&mut transaction, &state.ticketing).await?;

    let changed = sqlx::query(
        r#"
        UPDATE reward_draws
        SET status = 'scheduled', updated_at = now()
        WHERE workspace_id = $1
          AND id = $2
          AND status = 'draft'
          AND closes_at > now()
          AND draw_at >= closes_at
        "#,
    )
    .bind(workspace_id)
    .bind(draw_id)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?
    .rows_affected();
    if changed != 1 {
        return Err(CommerceError::Conflict);
    }
    transaction.commit().await.map_err(CommerceError::sqlx)?;
    load_reward_campaigns_filtered(state, Some(draw_id))
        .await?
        .into_iter()
        .next()
        .ok_or(CommerceError::Unexpected)
}

async fn cancel_reward_campaign_inner(
    state: &crate::AppState,
    draw_id: Uuid,
) -> Result<RewardCampaignView, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;
    configure_transaction(&mut transaction, &state.ticketing).await?;

    let row = sqlx::query_as::<_, (String, Uuid, Uuid)>(
        r#"
        SELECT draw.status, draw.reward_rule_id, allocation.reservation_id
        FROM reward_draws AS draw
        JOIN reward_draw_inventory_allocations AS allocation
          ON allocation.workspace_id = draw.workspace_id
         AND allocation.draw_id = draw.id
        WHERE draw.workspace_id = $1 AND draw.id = $2
        FOR UPDATE OF draw, allocation
        "#,
    )
    .bind(workspace_id)
    .bind(draw_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?
    .ok_or(CommerceError::NotFound)?;

    match row.0.as_str() {
        "cancelled" => {}
        "draft" | "scheduled" => {
            sqlx::query(
                "UPDATE reward_draws SET status = 'cancelled' WHERE workspace_id = $1 AND id = $2",
            )
            .bind(workspace_id)
            .bind(draw_id)
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
            sqlx::query(
                r#"
                UPDATE inventory_reservations
                SET status = 'released', released_at = now(),
                    release_reason = 'reward campaign cancelled'
                WHERE workspace_id = $1 AND id = $2 AND status = 'active'
                "#,
            )
            .bind(workspace_id)
            .bind(row.2)
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
            sqlx::query(
                "UPDATE reward_rules SET active = false WHERE workspace_id = $1 AND id = $2",
            )
            .bind(workspace_id)
            .bind(row.1)
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
        }
        _ => return Err(CommerceError::Conflict),
    }

    transaction.commit().await.map_err(CommerceError::sqlx)?;
    load_reward_campaigns_filtered(state, Some(draw_id))
        .await?
        .into_iter()
        .next()
        .ok_or(CommerceError::Unexpected)
}

async fn load_reward_draws(
    state: &crate::AppState,
) -> Result<Vec<RewardDrawAdminView>, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    sqlx::query_as::<_, RewardDrawAdminView>(
        r#"
        SELECT
            draw.id,
            draw.slug,
            draw.name,
            draw.prize_kind,
            draw.eligibility_kind,
            event.slug AS event_slug,
            draw.status,
            draw.winner_count,
            COALESCE(run_totals.run_count, 0)::bigint AS run_count,
            COALESCE(winner_totals.selected_winners, 0)::bigint AS selected_winners,
            COALESCE(proof_totals.proof_count, 0)::bigint AS proof_count,
            (
                draw.status IN ('draft', 'scheduled', 'cancelled')
                AND draw.completed_at IS NULL
                AND COALESCE(run_totals.run_count, 0) = 0
                AND COALESCE(winner_totals.selected_winners, 0) = 0
                AND COALESCE(proof_totals.proof_count, 0) = 0
            ) AS can_delete,
            draw.opens_at,
            draw.closes_at,
            draw.draw_at,
            draw.completed_at
        FROM reward_draws AS draw
        LEFT JOIN events AS event
          ON event.workspace_id = draw.workspace_id
         AND event.id = draw.event_id
        LEFT JOIN LATERAL (
            SELECT COUNT(*)::bigint AS run_count
            FROM reward_draw_runs AS run
            WHERE run.workspace_id = draw.workspace_id
              AND run.draw_id = draw.id
        ) AS run_totals ON true
        LEFT JOIN LATERAL (
            SELECT COUNT(*)::bigint AS selected_winners
            FROM reward_draw_winners AS winner
            WHERE winner.workspace_id = draw.workspace_id
              AND winner.draw_id = draw.id
        ) AS winner_totals ON true
        LEFT JOIN LATERAL (
            SELECT COUNT(*)::bigint AS proof_count
            FROM reward_draw_proofs AS proof
            WHERE proof.workspace_id = draw.workspace_id
              AND proof.draw_id = draw.id
        ) AS proof_totals ON true
        WHERE draw.workspace_id = $1
        ORDER BY draw.draw_at DESC, draw.id DESC
        "#,
    )
    .bind(workspace_id)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)
}

async fn delete_reward_draw_inner(
    state: &crate::AppState,
    draw_id: Uuid,
    request_id_value: Option<&str>,
) -> Result<DeletedRewardDrawView, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;
    configure_transaction(&mut transaction, &state.ticketing).await?;

    let draw = sqlx::query_as::<_, (String, String, String, Option<Uuid>, Option<OffsetDateTime>)>(
        r#"
        SELECT slug, status, prize_kind, reward_rule_id, completed_at
        FROM reward_draws
        WHERE workspace_id = $1 AND id = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(draw_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?
    .ok_or(CommerceError::NotFound)?;
    let (slug, status, prize_kind, reward_rule_id, completed_at) = draw;

    if !matches!(status.as_str(), "draft" | "scheduled" | "cancelled")
        || completed_at.is_some()
    {
        return Err(CommerceError::Conflict);
    }

    let durable_history = sqlx::query_as::<_, (bool, bool, bool)>(
        r#"
        SELECT
            EXISTS(
                SELECT 1 FROM reward_draw_runs
                WHERE workspace_id = $1 AND draw_id = $2
            ),
            EXISTS(
                SELECT 1 FROM reward_draw_winners
                WHERE workspace_id = $1 AND draw_id = $2
            ),
            EXISTS(
                SELECT 1 FROM reward_draw_proofs
                WHERE workspace_id = $1 AND draw_id = $2
            )
        "#,
    )
    .bind(workspace_id)
    .bind(draw_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;
    if durable_history.0 || durable_history.1 || durable_history.2 {
        return Err(CommerceError::Conflict);
    }

    let reservation_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT reservation_id
        FROM reward_draw_inventory_allocations
        WHERE workspace_id = $1 AND draw_id = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(draw_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    let deleted = sqlx::query(
        "DELETE FROM reward_draws WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace_id)
    .bind(draw_id)
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?
    .rows_affected();
    if deleted != 1 {
        return Err(CommerceError::Conflict);
    }

    if let Some(reservation_id) = reservation_id {
        sqlx::query(
            r#"
            DELETE FROM inventory_reservations AS reservation
            WHERE reservation.workspace_id = $1
              AND reservation.id = $2
              AND reservation.reservation_kind = 'campaign'
              AND reservation.external_reference = $3
              AND NOT EXISTS (
                  SELECT 1 FROM inventory_ledger AS ledger
                  WHERE ledger.workspace_id = reservation.workspace_id
                    AND ledger.reservation_id = reservation.id
              )
            "#,
        )
        .bind(workspace_id)
        .bind(reservation_id)
        .bind(format!("reward-draw:{draw_id}"))
        .execute(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;
    }

    if let Some(reward_rule_id) = reward_rule_id {
        let managed_rule_name = format!("campaign:{slug}");
        sqlx::query(
            r#"
            UPDATE reward_rules AS rule
            SET active = false
            WHERE rule.workspace_id = $1
              AND rule.id = $2
              AND rule.name = $3
              AND NOT EXISTS (
                  SELECT 1 FROM reward_draws AS other_draw
                  WHERE other_draw.workspace_id = rule.workspace_id
                    AND other_draw.reward_rule_id = rule.id
              )
              AND NOT EXISTS (
                  SELECT 1 FROM reward_grants AS grant
                  WHERE grant.workspace_id = rule.workspace_id
                    AND grant.reward_rule_id = rule.id
              )
            "#,
        )
        .bind(workspace_id)
        .bind(reward_rule_id)
        .bind(&managed_rule_name)
        .execute(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;

        sqlx::query(
            r#"
            DELETE FROM reward_rules AS rule
            WHERE rule.workspace_id = $1
              AND rule.id = $2
              AND rule.name = $3
              AND NOT EXISTS (
                  SELECT 1 FROM reward_draws AS other_draw
                  WHERE other_draw.workspace_id = rule.workspace_id
                    AND other_draw.reward_rule_id = rule.id
              )
              AND NOT EXISTS (
                  SELECT 1 FROM reward_grants AS grant
                  WHERE grant.workspace_id = rule.workspace_id
                    AND grant.reward_rule_id = rule.id
              )
            "#,
        )
        .bind(workspace_id)
        .bind(reward_rule_id)
        .bind(&managed_rule_name)
        .execute(&mut *transaction)
        .await
        .map_err(CommerceError::sqlx)?;
    }

    sqlx::query(
        r#"
        INSERT INTO audit_events (
            workspace_id, actor_kind, action, target_type, target_id, request_id, metadata
        )
        VALUES ($1, 'service', 'reward_draw.deleted', 'reward_draw', $2, $3, $4)
        "#,
    )
    .bind(workspace_id)
    .bind(draw_id.to_string())
    .bind(request_id_value)
    .bind(json!({
        "slug": &slug,
        "status": &status,
        "prize_kind": &prize_kind,
    }))
    .execute(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?;

    transaction.commit().await.map_err(CommerceError::sqlx)?;
    Ok(DeletedRewardDrawView {
        id: draw_id,
        slug,
        deleted: true,
    })
}

async fn load_reward_fulfillments(
    state: &crate::AppState,
) -> Result<Vec<RewardFulfillmentView>, CommerceError> {
    load_reward_fulfillments_filtered(state, None).await
}

async fn load_reward_fulfillments_filtered(
    state: &crate::AppState,
    winner_id: Option<Uuid>,
) -> Result<Vec<RewardFulfillmentView>, CommerceError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    sqlx::query_as::<_, RewardFulfillmentView>(
        r#"
        SELECT
            fulfillment.id,
            fulfillment.winner_id,
            fulfillment.draw_id,
            draw.slug AS draw_slug,
            winner.winner_rank,
            fan.display_name AS fan_display_name,
            CASE
                WHEN position('@' IN fan.normalized_email) > 1
                THEN left(fan.normalized_email, 1) || '***@' || split_part(fan.normalized_email, '@', 2)
                ELSE '***'
            END AS fan_email_masked,
            variant.sku AS prize_sku,
            product.name AS prize_name,
            variant.label AS prize_variant,
            fulfillment.quantity,
            fulfillment.status,
            fulfillment.created_at,
            fulfillment.updated_at
        FROM reward_draw_fulfillments AS fulfillment
        JOIN reward_draws AS draw
          ON draw.workspace_id = fulfillment.workspace_id
         AND draw.id = fulfillment.draw_id
        JOIN reward_draw_winners AS winner
          ON winner.workspace_id = fulfillment.workspace_id
         AND winner.id = fulfillment.winner_id
        JOIN fans AS fan
          ON fan.workspace_id = winner.workspace_id
         AND fan.id = winner.fan_id
        JOIN merch_variants AS variant
          ON variant.workspace_id = fulfillment.workspace_id
         AND variant.id = fulfillment.variant_id
        JOIN merch_products AS product
          ON product.workspace_id = variant.workspace_id
         AND product.id = variant.product_id
        WHERE fulfillment.workspace_id = $1
          AND ($2::uuid IS NULL OR fulfillment.winner_id = $2)
        ORDER BY fulfillment.created_at DESC, fulfillment.id DESC
        "#,
    )
    .bind(workspace_id)
    .bind(winner_id)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(CommerceError::sqlx)
}

async fn fulfill_reward_inner(
    state: &crate::AppState,
    winner_id: Uuid,
    payload: FulfillRewardRequest,
) -> Result<RewardFulfillmentView, CommerceError> {
    let status = clean_fulfillment_status(&payload.status)?;
    if status == "delivered" {
        require_inventory_writes(state).await?;
    }
    let actor_id = optional_text(payload.actor_id.as_deref(), 200)?;
    let note = optional_text(payload.note.as_deref(), 500)?;
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(CommerceError::sqlx)?;
    configure_transaction(&mut transaction, &state.ticketing).await?;

    let row = sqlx::query_as::<_, FulfillmentMutationRow>(
        r#"
        SELECT
            fulfillment.id,
            fulfillment.reward_grant_id,
            fulfillment.variant_id,
            allocation.reservation_id,
            fulfillment.quantity,
            fulfillment.status
        FROM reward_draw_fulfillments AS fulfillment
        JOIN reward_draw_inventory_allocations AS allocation
          ON allocation.workspace_id = fulfillment.workspace_id
         AND allocation.draw_id = fulfillment.draw_id
        WHERE fulfillment.workspace_id = $1 AND fulfillment.winner_id = $2
        FOR UPDATE OF fulfillment, allocation
        "#,
    )
    .bind(workspace_id)
    .bind(winner_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(CommerceError::sqlx)?
    .ok_or(CommerceError::NotFound)?;

    if row.status == status {
        transaction.commit().await.map_err(CommerceError::sqlx)?;
        return load_reward_fulfillments_filtered(state, Some(winner_id))
            .await?
            .into_iter()
            .next()
            .ok_or(CommerceError::Unexpected);
    }
    if matches!(row.status.as_str(), "delivered" | "cancelled") {
        return Err(CommerceError::Conflict);
    }

    match status.as_str() {
        "prepared" => {
            sqlx::query(
                r#"
                UPDATE reward_draw_fulfillments
                SET status = 'prepared', prepared_at = now(), actor_id = $3, note = $4
                WHERE workspace_id = $1 AND winner_id = $2
                "#,
            )
            .bind(workspace_id)
            .bind(winner_id)
            .bind(actor_id)
            .bind(note)
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
        }
        "delivered" => {
            sqlx::query(
                r#"
                INSERT INTO inventory_ledger (
                    workspace_id, variant_id, delta, movement_kind, idempotency_key,
                    reservation_id, actor_kind, actor_id, reason
                )
                VALUES ($1, $2, -$3, 'promotional_issue', $4, $5, 'staff', $6, $7)
                ON CONFLICT (workspace_id, idempotency_key) DO NOTHING
                "#,
            )
            .bind(workspace_id)
            .bind(row.variant_id)
            .bind(row.quantity)
            .bind(format!("reward-fulfillment:{}", row.id))
            .bind(row.reservation_id)
            .bind(actor_id.as_deref())
            .bind(note.as_deref().map_or("reward delivered", |value| value))
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
            consume_campaign_reservation_item(
                &mut transaction,
                workspace_id,
                row.reservation_id,
                row.variant_id,
                row.quantity,
            )
            .await?;
            sqlx::query(
                r#"
                UPDATE reward_draw_fulfillments
                SET status = 'delivered',
                    prepared_at = COALESCE(prepared_at, now()),
                    delivered_at = now(), actor_id = $3, note = $4
                WHERE workspace_id = $1 AND winner_id = $2
                "#,
            )
            .bind(workspace_id)
            .bind(winner_id)
            .bind(actor_id)
            .bind(note)
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
            sqlx::query(
                r#"
                UPDATE reward_grants
                SET status = 'delivered', delivered_at = COALESCE(delivered_at, now())
                WHERE workspace_id = $1 AND id = $2
                "#,
            )
            .bind(workspace_id)
            .bind(row.reward_grant_id)
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
        }
        "cancelled" => {
            consume_campaign_reservation_item(
                &mut transaction,
                workspace_id,
                row.reservation_id,
                row.variant_id,
                row.quantity,
            )
            .await?;
            sqlx::query(
                r#"
                UPDATE reward_draw_fulfillments
                SET status = 'cancelled', cancelled_at = now(), actor_id = $3, note = $4
                WHERE workspace_id = $1 AND winner_id = $2
                "#,
            )
            .bind(workspace_id)
            .bind(winner_id)
            .bind(actor_id)
            .bind(note)
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
            sqlx::query(
                r#"
                UPDATE reward_grants
                SET status = 'revoked', revoked_at = COALESCE(revoked_at, now())
                WHERE workspace_id = $1 AND id = $2
                "#,
            )
            .bind(workspace_id)
            .bind(row.reward_grant_id)
            .execute(&mut *transaction)
            .await
            .map_err(CommerceError::sqlx)?;
        }
        _ => return Err(CommerceError::Invalid),
    }

    finalize_campaign_reservation_if_empty(&mut transaction, workspace_id, row.reservation_id)
        .await?;
    transaction.commit().await.map_err(CommerceError::sqlx)?;
    load_reward_fulfillments_filtered(state, Some(winner_id))
        .await?
        .into_iter()
        .next()
        .ok_or(CommerceError::Unexpected)
}

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
