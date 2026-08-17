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
            draw.eligibility_ref,
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
