async fn load_drops(
    state: &crate::AppState,
    player_id: Option<Uuid>,
) -> Result<Vec<DropRow>, sqlx::Error> {
    sqlx::query_as::<_, DropRow>(
        r#"
        SELECT
            area_drop.id,
            area_drop.number,
            area_drop.city,
            area_drop.region,
            area_drop.signal_city_slug,
            area_drop.map_x,
            area_drop.map_y,
            area_drop.approximate_lat,
            area_drop.approximate_lng,
            area_drop.exact_lat,
            area_drop.exact_lng,
            area_drop.radius_meters,
            area_drop.max_claims,
            area_drop.starts_at,
            area_drop.ends_at,
            area_drop.clue_en,
            area_drop.clue_pl,
            area_drop.collectible_line,
            area_drop.collectible_track,
            area_drop.collectible_edition,
            area_drop.collectible_riddle,
            area_drop.active
                AND area_drop.exact_lat IS NOT NULL
                AND area_drop.exact_lng IS NOT NULL
                AND area_drop.starts_at <= now()
                AND area_drop.ends_at >= now() AS active_now,
            (
                SELECT count(*)::bigint
                FROM area_claims AS claim
                WHERE claim.workspace_id = area_drop.workspace_id
                  AND claim.drop_id = area_drop.id
            ) AS claim_count,
            CASE
                WHEN $2::uuid IS NULL THEN false
                ELSE EXISTS (
                    SELECT 1
                    FROM area_claims AS player_claim
                    WHERE player_claim.workspace_id = area_drop.workspace_id
                      AND player_claim.drop_id = area_drop.id
                      AND player_claim.player_id = $2
                )
            END AS player_claimed
        FROM area_drops AS area_drop
        WHERE area_drop.workspace_id = $1
        ORDER BY area_drop.number, area_drop.id
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(player_id)
    .fetch_all(state.ticketing.pool())
    .await
}

async fn load_claims(
    state: &crate::AppState,
    player_id: Uuid,
) -> Result<Vec<AreaClaim>, sqlx::Error> {
    let rows = sqlx::query_as::<_, ClaimRow>(
        r#"
        SELECT
            claim.drop_id,
            area_drop.number,
            area_drop.city,
            area_drop.collectible_line,
            area_drop.collectible_track,
            area_drop.collectible_edition,
            area_drop.collectible_riddle,
            claim.claimed_at,
            claim.distance_meters,
            claim.edition_number
        FROM area_claims AS claim
        INNER JOIN area_drops AS area_drop
          ON area_drop.workspace_id = claim.workspace_id
         AND area_drop.id = claim.drop_id
        WHERE claim.workspace_id = $1
          AND claim.player_id = $2
        ORDER BY claim.claimed_at, claim.drop_id
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(player_id)
    .fetch_all(state.ticketing.pool())
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let edition_number = u32::try_from(row.edition_number).ok();
            AreaClaim {
                drop_id: row.drop_id,
                number: row.number,
                city: row.city,
                line: row.collectible_line,
                track: row.collectible_track,
                edition: row.collectible_edition,
                riddle: row.collectible_riddle,
                claimed_at: row.claimed_at,
                distance_meters: u32::try_from(row.distance_meters).unwrap_or_default(),
                edition_number,
            }
        })
        .collect())
}


async fn load_vouchers(
    state: &crate::AppState,
    player_id: Uuid,
) -> Result<Vec<AreaVoucherPublic>, sqlx::Error> {
    sqlx::query_as::<_, AreaVoucherPublic>(
        r#"
        SELECT
            request_id,
            code,
            token_cost AS tokens,
            benefit,
            issued_at AS created_at,
            floor(extract(epoch FROM expires_at))::bigint AS expires_at,
            status,
            free_product_id,
            free_product_label,
            redeemed_at
        FROM area_reward_vouchers
        WHERE workspace_id = $1
          AND player_id = $2
          AND status IN ('issued', 'reserved', 'redeemed')
        ORDER BY issued_at DESC, id
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(player_id)
    .fetch_all(state.ticketing.pool())
    .await
}

async fn load_ticket_rewards(
    state: &crate::AppState,
    player_id: Uuid,
) -> Result<Vec<AreaTicketRewardPublic>, sqlx::Error> {
    sqlx::query_as::<_, AreaTicketRewardPublic>(
        r#"
        SELECT request_id, event_slug, credits, status, public_reference, issued_at
        FROM area_ticket_rewards
        WHERE workspace_id = $1 AND player_id = $2
        ORDER BY created_at DESC, id
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(player_id)
    .fetch_all(state.ticketing.pool())
    .await
}

async fn area_credit_balance(
    state: &crate::AppState,
    player_id: Uuid,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COALESCE(sum(delta), 0)::bigint
        FROM area_credit_ledger
        WHERE workspace_id = $1 AND player_id = $2
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(player_id)
    .fetch_one(state.ticketing.pool())
    .await
}

async fn wallet_for_player(
    state: &crate::AppState,
    player_id: Uuid,
) -> Result<AreaWallet, sqlx::Error> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let legacy_migration = sqlx::query_scalar::<_, bool>(
        r#"SELECT EXISTS (SELECT 1 FROM area_legacy_wallet_imports WHERE workspace_id=$1 AND player_id=$2)"#,
    )
    .bind(workspace_id)
    .bind(player_id)
    .fetch_one(state.ticketing.pool());
    let (drops, claims, token_balance, legacy_migration_applied, vouchers, ticket_rewards) =
        tokio::try_join!(
            load_drops(state, Some(player_id)),
            load_claims(state, player_id),
            area_credit_balance(state, player_id),
            legacy_migration,
            load_vouchers(state, player_id),
            load_ticket_rewards(state, player_id),
        )?;
    let total = u32::try_from(drops.len()).unwrap_or(u32::MAX);
    let current =
        u32::try_from(drops.iter().filter(|drop| drop.claim_count > 0).count()).unwrap_or(u32::MAX);
    let percent = if total == 0 {
        0.0
    } else {
        f64::from(current) * 100.0 / f64::from(total)
    };
    let token_balance = safe_u32(token_balance);
    let public_drops = drops.iter().map(public_drop).collect::<Vec<_>>();
    let live_drops = public_drops
        .iter()
        .filter(|drop| drop.active && !drop.full)
        .map(|drop| LiveDrop {
            id: drop.id.clone(),
        })
        .collect();

    Ok(AreaWallet {
        authenticated: true,
        migration_required: false,
        legacy_migration_applied,
        token_balance,
        reward_credits: token_balance,
        reward: RewardSummary {
            credits_per_code: 1,
            benefit: "free-item-and-shipping",
        },
        collection_size: total,
        community: AreaCommunity {
            current,
            total,
            percent,
        },
        claims,
        vouchers,
        ticket_rewards,
        live_drops,
        drops: public_drops,
    })
}

async fn upsert_player(
    state: &crate::AppState,
    normalized_email: &str,
    fan_id: Option<Uuid>,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO area_players (workspace_id, normalized_email, fan_id)
        VALUES ($1, $2, $3)
        ON CONFLICT (workspace_id, normalized_email) DO UPDATE
        SET fan_id = COALESCE(area_players.fan_id, EXCLUDED.fan_id),
            last_seen_at = now()
        RETURNING id
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(normalized_email)
    .bind(fan_id)
    .fetch_one(state.ticketing.pool())
    .await
}

async fn mobile_player(
    state: &crate::AppState,
    headers: &HeaderMap,
) -> Result<Option<Uuid>, sqlx::Error> {
    let Some(session) = fan_session_from_headers(headers) else {
        return Ok(None);
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let fan_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        UPDATE fan_sessions
        SET last_seen_at = now()
        WHERE workspace_id = $1
          AND session_token_hash = digest($2, 'sha256')
          AND revoked_at IS NULL
          AND expires_at > now()
        RETURNING fan_id
        "#,
    )
    .bind(workspace_id)
    .bind(session.as_str())
    .fetch_optional(state.ticketing.pool())
    .await?;
    let Some(fan_id) = fan_id else {
        return Ok(None);
    };
    let email = sqlx::query_scalar::<_, String>(
        r#"
        SELECT normalized_email
        FROM fans
        WHERE workspace_id = $1
          AND id = $2
          AND status = 'active'
        "#,
    )
    .bind(workspace_id)
    .bind(fan_id)
    .fetch_optional(state.ticketing.pool())
    .await?;
    match email {
        Some(email) => upsert_player(state, &email, Some(fan_id)).await.map(Some),
        None => Ok(None),
    }
}

async fn player_exists(state: &crate::AppState, player_id: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM area_players
            WHERE workspace_id = $1 AND id = $2
        )
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(player_id)
    .fetch_one(state.ticketing.pool())
    .await
}
