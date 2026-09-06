/// Returns the public tenant AREA catalogue without exact claim coordinates.
pub async fn public_drops(State(state): State<crate::AppState>) -> Response {
    match load_drops(&state, None).await {
        Ok(rows) => {
            let total = u32::try_from(rows.len()).unwrap_or(u32::MAX);
            let current = u32::try_from(rows.iter().filter(|drop| drop.claim_count > 0).count())
                .unwrap_or(u32::MAX);
            let percent = if total == 0 {
                0.0
            } else {
                f64::from(current) * 100.0 / f64::from(total)
            };
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PUBLIC_AREA_CACHE)],
                Json(PublicDropsResponse {
                    items: rows.into_iter().map(public_drop).collect(),
                    community: AreaCommunity {
                        current,
                        total,
                        percent,
                    },
                }),
            )
                .into_response()
        }
        Err(error) => {
            tracing::warn!(%error, "AREA public catalogue unavailable");
            temporary()
        }
    }
}

/// Links a website AREA account to the same canonical player identity as a fan session.
pub async fn link_player(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<LinkPlayerRequest>, JsonRejection>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Unauthorized.");
    }
    if !valid_idempotency_key(&headers) {
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
    let Some(email) = normalize_email(&payload.email) else {
        return error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "INVALID_REQUEST",
            "Invalid email.",
        );
    };
    match upsert_player(&state, &email, None).await {
        Ok(player_id) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(LinkPlayerResponse { player_id }),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "AREA player link failed");
            temporary()
        }
    }
}

/// Returns the AREA wallet for an authenticated Virya Signal fan session.
pub async fn me_wallet(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    match mobile_player(&state, &headers).await {
        Ok(Some(player_id)) => match wallet_for_player(&state, player_id).await {
            Ok(wallet) => (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(wallet),
            )
                .into_response(),
            Err(error) => {
                tracing::error!(%error, "AREA mobile wallet failed");
                temporary()
            }
        },
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

/// Returns the AREA wallet for a linked website player.
pub async fn internal_wallet(
    State(state): State<crate::AppState>,
    Path(player_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Unauthorized.");
    }
    match player_exists(&state, player_id).await {
        Ok(true) => match wallet_for_player(&state, player_id).await {
            Ok(wallet) => (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(wallet),
            )
                .into_response(),
            Err(error) => {
                tracing::error!(%error, "AREA internal wallet failed");
                temporary()
            }
        },
        Ok(false) => error_response(StatusCode::NOT_FOUND, "NOT_FOUND", "Player not found."),
        Err(error) => {
            tracing::error!(%error, "AREA player lookup failed");
            temporary()
        }
    }
}

async fn next_edition_number(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    drop_id: &str,
    max_claims: i32,
    preferred: Option<u32>,
) -> Result<Option<i32>, sqlx::Error> {
    if let Some(preferred) = preferred
        .and_then(|value| i32::try_from(value).ok())
        .filter(|value| (1..=max_claims).contains(value))
    {
        let available = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT NOT EXISTS (
                SELECT 1
                FROM area_claims
                WHERE workspace_id = $1
                  AND drop_id = $2
                  AND edition_number = $3
            )
            "#,
        )
        .bind(workspace_id)
        .bind(drop_id)
        .bind(preferred)
        .fetch_one(&mut **transaction)
        .await?;
        if available {
            return Ok(Some(preferred));
        }
    }

    sqlx::query_scalar::<_, i32>(
        r#"
        SELECT candidate::integer
        FROM generate_series(1, $3::integer) AS candidate
        WHERE NOT EXISTS (
            SELECT 1
            FROM area_claims
            WHERE workspace_id = $1
              AND drop_id = $2
              AND edition_number = candidate
        )
        ORDER BY candidate
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .bind(drop_id)
    .bind(max_claims)
    .fetch_optional(&mut **transaction)
    .await
}
