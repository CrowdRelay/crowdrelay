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
            draw.eligibility_ref,
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
    let eligibility_ref = payload
        .eligibility_ref
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
        "all_active" | "synesthesia_completion" => None,
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
            id, workspace_id, slug, name, prize_kind, eligibility_kind, eligibility_ref,
            event_id, reward_rule_id, winner_count, base_entries,
            entries_per_referral, entries_per_checkin, max_entries,
            claim_expires_hours, opens_at, closes_at, draw_at, status
        )
        VALUES (
            $1, $2, $3, $4, 'physical_item', $5, $6,
            $7, $8, $9, $10, $11, $12, $13,
            $14, $15, $16, $17, $18
        )
        "#,
    )
    .bind(draw_id)
    .bind(workspace_id)
    .bind(&slug)
    .bind(name)
    .bind(&payload.eligibility_kind)
    .bind(eligibility_ref.as_deref())
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
