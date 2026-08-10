/// Returns the public 12-drop catalogue without exact claim coordinates.
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
                    items: rows.iter().map(public_drop).collect(),
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

/// Imports claims created by the pre-backend website ledger.
///
/// This route is internal and idempotent. It preserves original timestamps and
/// edition numbers where they are still available, while never exceeding a
/// drop's canonical capacity.
pub async fn internal_import_claims(
    State(state): State<crate::AppState>,
    Path(player_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<ImportClaimsRequest>, JsonRejection>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Unauthorized.");
    }
    crate::http_metrics().record_legacy_area_claim_import();
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
                "Invalid import request.",
            );
        }
    };
    if payload.claims.len() > 12
        || payload.claims.iter().any(|claim| {
            !valid_drop_id(&claim.drop_id)
                || claim
                    .edition_number
                    .is_some_and(|number| number == 0 || number > 500)
                || claim
                    .claimed_at
                    .is_some_and(|claimed_at| claimed_at > OffsetDateTime::now_utc())
        })
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "Invalid legacy claims.",
        );
    }
    let mut unique = HashSet::with_capacity(payload.claims.len());
    if payload
        .claims
        .iter()
        .any(|claim| !unique.insert(claim.drop_id.clone()))
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "Duplicate legacy claims are not allowed.",
        );
    }
    match player_exists(&state, player_id).await {
        Ok(true) => {}
        Ok(false) => {
            return error_response(StatusCode::NOT_FOUND, "NOT_FOUND", "Player not found.");
        }
        Err(error) => {
            tracing::error!(%error, "AREA legacy import player lookup failed");
            return temporary();
        }
    }

    let mut claims = payload.claims;
    claims.sort_by(|left, right| left.drop_id.cmp(&right.drop_id));
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = match state.ticketing.pool().begin().await {
        Ok(transaction) => transaction,
        Err(error) => {
            tracing::error!(%error, "AREA legacy import transaction failed to start");
            return temporary();
        }
    };
    match lock_area_player(&mut transaction, workspace_id, player_id).await {
        Ok(true) => {}
        Ok(false) => return error_response(StatusCode::NOT_FOUND, "NOT_FOUND", "Player not found."),
        Err(error) => {
            tracing::error!(%error, "AREA legacy import player lock failed");
            return temporary();
        }
    }

    for claim in claims {
        let drop = match lock_drop(&mut transaction, workspace_id, &claim.drop_id, player_id).await
        {
            Ok(Some(drop)) => drop,
            Ok(None) => {
                return error_response(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "DROP_UNKNOWN",
                    "A legacy drop no longer exists.",
                );
            }
            Err(error) => {
                tracing::error!(%error, drop_id = claim.drop_id, "AREA legacy import drop lock failed");
                return temporary();
            }
        };
        match existing_claim(&mut transaction, workspace_id, player_id, &claim.drop_id).await {
            Ok(Some(_)) => continue,
            Ok(None) => {}
            Err(error) => {
                tracing::error!(%error, drop_id = claim.drop_id, "AREA legacy import claim lookup failed");
                return temporary();
            }
        }
        let edition_number = match next_edition_number(
            &mut transaction,
            workspace_id,
            &claim.drop_id,
            drop.max_claims,
            claim.edition_number,
        )
        .await
        {
            Ok(Some(number)) => number,
            Ok(None) => {
                return error_response(
                    StatusCode::CONFLICT,
                    "DROP_FULL",
                    "A legacy drop has reached its claim limit.",
                );
            }
            Err(error) => {
                tracing::error!(%error, drop_id = claim.drop_id, "AREA legacy edition allocation failed");
                return temporary();
            }
        };
        let now = OffsetDateTime::now_utc();
        let fallback_claimed_at = now.max(drop.starts_at).min(drop.ends_at);
        let claimed_at = claim
            .claimed_at
            .filter(|value| *value >= drop.starts_at && *value <= drop.ends_at)
            .unwrap_or(fallback_claimed_at);
        let inserted = sqlx::query(
            r#"
            INSERT INTO area_claims (
                workspace_id, player_id, drop_id, claimed_at,
                distance_meters, edition_number, claim_source
            )
            VALUES ($1, $2, $3, $4, 0, $5, 'legacy_import')
            ON CONFLICT (workspace_id, player_id, drop_id) DO NOTHING
            "#,
        )
        .bind(workspace_id)
        .bind(player_id)
        .bind(&claim.drop_id)
        .bind(claimed_at)
        .bind(edition_number)
        .execute(&mut *transaction)
        .await;
        match inserted {
            Ok(result) if result.rows_affected() == 1 => {
                let reference = format!("claim:{}", claim.drop_id);
                if let Err(error) = insert_credit_delta(
                    &mut transaction,
                    workspace_id,
                    player_id,
                    1,
                    "claim",
                    &reference,
                    Some(claimed_at),
                )
                .await
                {
                    tracing::error!(%error, drop_id = claim.drop_id, "AREA legacy credit insert failed");
                    return temporary();
                }
            }
            Ok(result) if result.rows_affected() == 0 => {}
            Ok(_) => return temporary(),
            Err(error) => {
                tracing::error!(%error, drop_id = claim.drop_id, "AREA legacy import insert failed");
                return temporary();
            }
        }
    }

    if let Err(error) = transaction.commit().await {
        tracing::error!(%error, "AREA legacy import commit failed");
        return temporary();
    }
    match wallet_for_player(&state, player_id).await {
        Ok(wallet) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(wallet),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "AREA wallet reload after legacy import failed");
            temporary()
        }
    }
}
