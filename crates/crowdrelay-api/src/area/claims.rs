async fn lock_drop(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    drop_id: &str,
    player_id: Uuid,
    require_enabled: bool,
) -> Result<Option<DropRow>, sqlx::Error> {
    let locked = sqlx::query_scalar::<_, String>(
        r#"
        SELECT id
        FROM area_drops
        WHERE workspace_id = $1
          AND id = $2
          AND (
              NOT $3
              OR (
                  published_at IS NOT NULL
                  AND archived_at IS NULL
                  AND EXISTS (
                      SELECT 1
                      FROM area_workspace_settings AS area_settings
                      WHERE area_settings.workspace_id = area_drops.workspace_id
                        AND area_settings.enabled
                  )
              )
          )
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(drop_id)
    .bind(require_enabled)
    .fetch_optional(&mut **transaction)
    .await?;
    if locked.is_none() {
        return Ok(None);
    }

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
            EXISTS (
                SELECT 1
                FROM area_claims AS player_claim
                WHERE player_claim.workspace_id = area_drop.workspace_id
                  AND player_claim.drop_id = area_drop.id
                  AND player_claim.player_id = $3
            ) AS player_claimed
        FROM area_drops AS area_drop
        WHERE area_drop.workspace_id = $1 AND area_drop.id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(drop_id)
    .bind(player_id)
    .fetch_optional(&mut **transaction)
    .await
}

async fn existing_claim(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    player_id: Uuid,
    drop_id: &str,
) -> Result<Option<ExistingClaim>, sqlx::Error> {
    sqlx::query_as::<_, ExistingClaim>(
        r#"
        SELECT
            claim.drop_id,
            area_drop.number,
            area_drop.city,
            area_drop.collectible_line,
            area_drop.collectible_track,
            area_drop.collectible_edition,
            area_drop.collectible_riddle
        FROM area_claims AS claim
        INNER JOIN area_drops AS area_drop
          ON area_drop.workspace_id = claim.workspace_id
         AND area_drop.id = claim.drop_id
        WHERE claim.workspace_id = $1
          AND claim.player_id = $2
          AND claim.drop_id = $3
        "#,
    )
    .bind(workspace_id)
    .bind(player_id)
    .bind(drop_id)
    .fetch_optional(&mut **transaction)
    .await
}

async fn claim_drop(
    state: &crate::AppState,
    player_id: Uuid,
    headers: &HeaderMap,
    payload: Result<Json<ClaimRequest>, JsonRejection>,
) -> Response {
    if !valid_idempotency_key(headers) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "A valid Idempotency-Key is required.",
        );
    }
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                "Invalid request.",
            );
        }
    };
    if !valid_drop_id(&payload.drop_id)
        || payload.challenge.len() < 40
        || payload.challenge.len() > 512
        || !valid_samples(&payload.samples)
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "Invalid claim data.",
        );
    }

    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = match state.ticketing.pool().begin().await {
        Ok(transaction) => transaction,
        Err(error) => {
            tracing::error!(%error, "AREA claim transaction failed to start");
            return temporary();
        }
    };
    match lock_area_player(&mut transaction, workspace_id, player_id).await {
        Ok(true) => {}
        Ok(false) => return error_response(StatusCode::NOT_FOUND, "NOT_FOUND", "Player not found."),
        Err(error) => {
            tracing::error!(%error, "AREA player lock failed");
            return temporary();
        }
    }
    match existing_claim(&mut transaction, workspace_id, player_id, &payload.drop_id).await {
        Ok(Some(existing)) => {
            if transaction.commit().await.is_err() {
                return temporary();
            }
            return (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(ClaimResponse {
                    ok: true,
                    already_claimed: true,
                    collectible: Some(collectible_from_existing(existing)),
                    reward_credits_awarded: 0,
                }),
            )
                .into_response();
        }
        Ok(None) => {}
        Err(error) => {
            tracing::error!(%error, "AREA existing claim lookup failed");
            return temporary();
        }
    }

    let challenge = sqlx::query_as::<_, LockedChallenge>(
        r#"
        UPDATE area_challenges
        SET consumed_at = now()
        WHERE workspace_id = $1
          AND player_id = $2
          AND token_hash = $3
          AND consumed_at IS NULL
          AND expires_at > now()
        RETURNING issued_at, expires_at, drop_id
        "#,
    )
    .bind(workspace_id)
    .bind(player_id)
    .bind(token_hash(&payload.challenge))
    .fetch_optional(&mut *transaction)
    .await;
    let challenge = match challenge {
        Ok(Some(challenge)) if challenge.drop_id == payload.drop_id => challenge,
        Ok(_) => {
            return error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "CHALLENGE_INVALID",
                "The location challenge is invalid or expired.",
            );
        }
        Err(error) => {
            tracing::error!(%error, "AREA challenge consumption failed");
            return temporary();
        }
    };
    if !challenge_time_valid(&challenge, &payload.samples) {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "NOT_ENOUGH_SAMPLES",
            "Not enough fresh location samples.",
        );
    }

    let drop = match lock_drop(&mut transaction, workspace_id, &payload.drop_id, player_id, true).await {
        Ok(Some(drop)) => drop,
        Ok(None) => {
            return error_response(
                StatusCode::CONFLICT,
                "DROP_INACTIVE",
                "This drop is not active.",
            );
        }
        Err(error) => {
            tracing::error!(%error, "AREA drop lock failed");
            return temporary();
        }
    };
    if !drop.active_now {
        return error_response(
            StatusCode::CONFLICT,
            "DROP_INACTIVE",
            "This drop is not active.",
        );
    }
    if drop.claim_count >= i64::from(drop.max_claims) {
        return error_response(
            StatusCode::CONFLICT,
            "DROP_FULL",
            "This drop has reached its claim limit.",
        );
    }
    let distance = match location_evaluation(&drop, &payload.samples) {
        Ok(distance) => distance,
        Err("DROP_INACTIVE") => {
            return error_response(
                StatusCode::CONFLICT,
                "DROP_INACTIVE",
                "This drop is not configured.",
            );
        }
        Err("LOW_ACCURACY") => {
            return error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "LOW_ACCURACY",
                "Location is not accurate enough. Move outdoors and retry.",
            );
        }
        Err("OUTSIDE_ZONE") => {
            return error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "OUTSIDE_ZONE",
                "You are outside the drop zone.",
            );
        }
        Err(_) => {
            return error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "NOT_ENOUGH_SAMPLES",
                "Not enough location samples.",
            );
        }
    };
    let edition_number = match next_edition_number(
        &mut transaction,
        workspace_id,
        &payload.drop_id,
        drop.max_claims,
        None,
    )
    .await
    {
        Ok(Some(number)) => number,
        Ok(None) => {
            return error_response(
                StatusCode::CONFLICT,
                "DROP_FULL",
                "This drop has reached its claim limit.",
            );
        }
        Err(error) => {
            tracing::error!(%error, "AREA edition allocation failed");
            return temporary();
        }
    };
    let inserted = sqlx::query(
        r#"
        INSERT INTO area_claims (
            workspace_id, player_id, drop_id, distance_meters,
            edition_number, claim_source
        )
        VALUES ($1, $2, $3, $4, $5, 'gps')
        ON CONFLICT (workspace_id, player_id, drop_id) DO NOTHING
        "#,
    )
    .bind(workspace_id)
    .bind(player_id)
    .bind(&payload.drop_id)
    .bind(distance)
    .bind(edition_number)
    .execute(&mut *transaction)
    .await;
    let inserted = match inserted {
        Ok(result) => result.rows_affected() == 1,
        Err(error) => {
            tracing::error!(%error, "AREA claim insert failed");
            return temporary();
        }
    };
    if !inserted {
        match existing_claim(&mut transaction, workspace_id, player_id, &payload.drop_id).await {
            Ok(Some(existing)) => {
                if let Err(error) = transaction.commit().await {
                    tracing::error!(%error, "AREA concurrent claim commit failed");
                    return temporary();
                }
                return (
                    StatusCode::OK,
                    [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                    Json(ClaimResponse {
                        ok: true,
                        already_claimed: true,
                        collectible: Some(collectible_from_existing(existing)),
                        reward_credits_awarded: 0,
                    }),
                )
                    .into_response();
            }
            Ok(None) => {
                return error_response(
                    StatusCode::CONFLICT,
                    "CLAIM_CONFLICT",
                    "The claim was already processed. Refresh progress.",
                );
            }
            Err(error) => {
                tracing::error!(%error, "AREA concurrent claim reload failed");
                return temporary();
            }
        }
    }
    let credit_reference = format!("claim:{}", payload.drop_id);
    if let Err(error) = insert_credit_delta(
        &mut transaction,
        workspace_id,
        player_id,
        1,
        "claim",
        &credit_reference,
        None,
    )
    .await
    {
        tracing::error!(%error, "AREA claim credit ledger insert failed");
        return temporary();
    }
    if let Err(error) = transaction.commit().await {
        tracing::error!(%error, "AREA claim commit failed");
        return temporary();
    }

    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(ClaimResponse {
            ok: true,
            already_claimed: false,
            collectible: Some(AreaCollectible {
                drop_id: drop.id,
                number: drop.number,
                city: drop.city,
                line: drop.collectible_line,
                track: drop.collectible_track,
                edition: drop.collectible_edition,
                riddle: drop.collectible_riddle,
            }),
            reward_credits_awarded: 1,
        }),
    )
        .into_response()
}

pub async fn me_claim(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ClaimRequest>, JsonRejection>,
) -> Response {
    match mobile_player(&state, &headers).await {
        Ok(Some(player_id)) => claim_drop(&state, player_id, &headers, payload).await,
        Ok(None) => error_response(
            StatusCode::UNAUTHORIZED,
            "AUTH_REQUIRED",
            "Sign in required.",
        ),
        Err(error) => {
            tracing::warn!(%error, "AREA fan session lookup failed");
            temporary()
        }
    }
}

pub async fn internal_claim(
    State(state): State<crate::AppState>,
    Path(player_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<ClaimRequest>, JsonRejection>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Unauthorized.");
    }
    match player_exists(&state, player_id).await {
        Ok(true) => claim_drop(&state, player_id, &headers, payload).await,
        Ok(false) => error_response(StatusCode::NOT_FOUND, "NOT_FOUND", "Player not found."),
        Err(error) => {
            tracing::error!(%error, "AREA player lookup failed");
            temporary()
        }
    }
}
