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
