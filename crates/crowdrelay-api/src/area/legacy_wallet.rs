fn valid_migration_id(value: &str) -> bool {
    let value = value.trim();
    value.len() >= 8
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.'))
}

fn normalize_import_voucher_status(voucher: &LegacyVoucherImport) -> &'static str {
    match voucher.status.as_str() {
        "redeemed" if voucher.redeemed_at.is_some() => "redeemed",
        "reserved" if voucher.reservation_id.is_some() && voucher.reserved_until.is_some() => "reserved",
        _ => "issued",
    }
}

pub async fn internal_import_legacy_wallet(
    State(state): State<crate::AppState>,
    Path(player_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<ImportLegacyWalletRequest>, JsonRejection>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Unauthorized.");
    }
    match crate::ecosystem::feature_enabled(&state, "area_legacy_imports_enabled").await {
        Ok(true) => {}
        Ok(false) => return error_response(StatusCode::GONE, "AREA_LEGACY_IMPORTS_DISABLED", "Legacy AREA imports are disabled."),
        Err(_) => return error_response(StatusCode::SERVICE_UNAVAILABLE, "AREA_LEGACY_IMPORT_GATE_UNAVAILABLE", "Legacy AREA import gate is unavailable."),
    }
    crate::http_metrics().record_legacy_area_wallet_import_attempt();
    if !valid_idempotency_key(&headers) {
        return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "A valid Idempotency-Key is required.");
    }
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Invalid legacy wallet import."),
    };
    if !valid_migration_id(&payload.migration_id)
        || payload.token_balance > 10_000
        || payload.vouchers.len() > 100
        || payload.ticket_rewards.len() > 100
    {
        return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Invalid legacy wallet import.");
    }
    let mut voucher_request_ids = HashSet::with_capacity(payload.vouchers.len());
    let mut ticket_request_ids = HashSet::with_capacity(payload.ticket_rewards.len());
    if payload.vouchers.iter().any(|voucher| {
        !voucher_request_ids.insert(voucher.request_id)
            || voucher.tokens != 1
            || voucher.benefit != "free-item-and-shipping"
            || !matches!(voucher.status.as_str(), "issued" | "reserved" | "redeemed")
            || normalize_reward_code(&voucher.code).is_none()
            || voucher.expires_at <= voucher.created_at.unix_timestamp()
    }) || payload.ticket_rewards.iter().any(|reward| {
        !ticket_request_ids.insert(reward.request_id)
            || reward.credits == 0
            || reward.credits > 20
            || !valid_event_slug(&reward.event_slug)
            || normalize_email(&reward.fan_email).is_none()
            || reward.public_reference.as_deref().is_none_or(|value| !valid_small_text(value, 128))
            || reward.issued_at.is_none()
    }) {
        return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Invalid legacy wallet records.");
    }

    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(tx) => tx,
        Err(error) => {
            tracing::error!(%error, "AREA legacy wallet transaction failed to start");
            return temporary();
        }
    };
    match lock_area_player(&mut tx, workspace_id, player_id).await {
        Ok(true) => {}
        Ok(false) => return error_response(StatusCode::NOT_FOUND, "NOT_FOUND", "Player not found."),
        Err(error) => {
            tracing::error!(%error, "AREA legacy wallet player lock failed");
            return temporary();
        }
    }
    let existing_import = sqlx::query_as::<_, (String, i32, i32, i32)>(
        r#"SELECT migration_id, source_balance, source_voucher_count, source_ticket_reward_count FROM area_legacy_wallet_imports WHERE workspace_id=$1 AND player_id=$2"#,
    )
    .bind(workspace_id)
    .bind(player_id)
    .fetch_optional(&mut *tx)
    .await;
    match existing_import {
        Ok(Some((migration_id, source_balance, source_voucher_count, source_ticket_reward_count))) => {
            if migration_id != payload.migration_id
                || source_balance != payload.token_balance as i32
                || source_voucher_count != payload.vouchers.len() as i32
                || source_ticket_reward_count != payload.ticket_rewards.len() as i32
            {
                return error_response(StatusCode::CONFLICT, "MIGRATION_ALREADY_APPLIED", "A different legacy wallet was already imported.");
            }
            if tx.commit().await.is_err() { return temporary(); }
            return match wallet_for_player(&state, player_id).await {
                Ok(wallet) => (StatusCode::OK, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(wallet)).into_response(),
                Err(_) => temporary(),
            };
        }
        Ok(None) => {}
        Err(error) => {
            tracing::error!(%error, "AREA legacy wallet marker lookup failed");
            return temporary();
        }
    }

    let current_balance = match area_credit_balance_tx(&mut tx, workspace_id, player_id).await {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(%error, "AREA legacy wallet balance lookup failed");
            return temporary();
        }
    };
    let target_balance = i64::from(payload.token_balance);
    let adjustment = target_balance - current_balance;
    if adjustment != 0 {
        let delta = match i32::try_from(adjustment) {
            Ok(value) => value,
            Err(_) => return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Legacy balance is outside the supported range."),
        };
        let reference = format!("legacy-balance:{}", payload.migration_id);
        if let Err(error) = insert_credit_delta(
            &mut tx,
            workspace_id,
            player_id,
            delta,
            "legacy_balance_import",
            &reference,
            None,
        )
        .await
        {
            tracing::error!(%error, "AREA legacy wallet balance import failed");
            return temporary();
        }
    }

    for voucher in &payload.vouchers {
        let Some(code) = normalize_reward_code(&voucher.code) else {
            return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Invalid legacy voucher code.");
        };
        let hash = area_reward_code_hash(&code);
        let status = normalize_import_voucher_status(voucher);
        let reserved_until = match (status, voucher.reserved_until) {
            ("reserved", Some(millis)) => match OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000) {
                Ok(value) => Some(value),
                Err(_) => return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Invalid legacy voucher lease timestamp."),
            },
            _ => None,
        };
        let result = sqlx::query(
            r#"
            INSERT INTO area_reward_vouchers (
                workspace_id, player_id, request_id, code, code_hash, code_suffix,
                token_cost, benefit, status, issued_at, expires_at,
                reservation_id, reserved_until, checkout_session_id,
                free_product_id, free_product_label, redeemed_at, updated_at
            )
            VALUES (
                $1,$2,$3,$4,$5,$6,1,'free-item-and-shipping',$7,$8,to_timestamp($9),
                $10,$11,$12,$13,$14,$15,now()
            )
            ON CONFLICT (workspace_id, request_id) DO NOTHING
            "#,
        )
        .bind(workspace_id)
        .bind(player_id)
        .bind(voucher.request_id)
        .bind(&code)
        .bind(hash)
        .bind(code.get(code.len().saturating_sub(4)..).unwrap_or_default())
        .bind(status)
        .bind(voucher.created_at)
        .bind(voucher.expires_at)
        .bind(if status == "reserved" { voucher.reservation_id.as_deref() } else { None })
        .bind(if status == "reserved" { reserved_until } else { None })
        .bind(if status == "reserved" { voucher.checkout_session_id.as_deref() } else { None })
        .bind(voucher.free_product_id.as_deref())
        .bind(voucher.free_product_label.as_deref())
        .bind(if status == "redeemed" { voucher.redeemed_at } else { None })
        .execute(&mut *tx)
        .await;
        match result {
            Ok(result) if result.rows_affected() == 1 => {}
            Ok(_) => {
                let existing = sqlx::query_as::<_, (Uuid, String)>(
                    "SELECT player_id, code FROM area_reward_vouchers WHERE workspace_id=$1 AND request_id=$2",
                )
                .bind(workspace_id)
                .bind(voucher.request_id)
                .fetch_optional(&mut *tx)
                .await;
                match existing {
                    Ok(Some((existing_player, existing_code))) if existing_player == player_id && existing_code == code => {}
                    Ok(_) => return error_response(StatusCode::CONFLICT, "LEGACY_IMPORT_CONFLICT", "Legacy voucher request id conflicts with canonical state."),
                    Err(error) => {
                        tracing::error!(%error, request_id=%voucher.request_id, "AREA legacy voucher reconciliation failed");
                        return temporary();
                    }
                }
            }
            Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("23505") => {
                return error_response(StatusCode::CONFLICT, "LEGACY_IMPORT_CONFLICT", "Legacy voucher conflicts with canonical state.");
            }
            Err(error) => {
                tracing::error!(%error, request_id=%voucher.request_id, "AREA legacy voucher import failed");
                return temporary();
            }
        }
    }

    for reward in &payload.ticket_rewards {
        let Some(issued_at) = reward.issued_at else {
            return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Legacy ticket reward requires an issue timestamp.");
        };
        let Some(email) = normalize_email(&reward.fan_email) else {
            return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Legacy ticket reward email is invalid.");
        };
        let credits = match i32::try_from(reward.credits) {
            Ok(value @ 1..=20) => value,
            _ => return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Legacy ticket reward credits are invalid."),
        };
        let result = sqlx::query(
            r#"
            INSERT INTO area_ticket_rewards (
                workspace_id, player_id, request_id, event_slug, credits, fan_email,
                status, reservation_id, reservation_expires_at,
                public_reference, issued_at, created_at, updated_at
            )
            VALUES ($1,$2,$3,$4,$5,$6,'issued','legacy-import',$7,$8,$9,$9,now())
            ON CONFLICT (workspace_id, request_id) DO NOTHING
            "#,
        )
        .bind(workspace_id)
        .bind(player_id)
        .bind(reward.request_id)
        .bind(&reward.event_slug)
        .bind(credits)
        .bind(email)
        .bind(issued_at + Duration::seconds(1))
        .bind(reward.public_reference.as_deref())
        .bind(issued_at)
        .execute(&mut *tx)
        .await;
        match result {
            Ok(result) if result.rows_affected() == 1 => {}
            Ok(_) => {
                let existing = sqlx::query_as::<_, (Uuid, String, Option<String>)>(
                    "SELECT player_id, event_slug, public_reference FROM area_ticket_rewards WHERE workspace_id=$1 AND request_id=$2",
                )
                .bind(workspace_id)
                .bind(reward.request_id)
                .fetch_optional(&mut *tx)
                .await;
                match existing {
                    Ok(Some((existing_player, existing_event, existing_reference)))
                        if existing_player == player_id
                            && existing_event == reward.event_slug
                            && existing_reference.as_deref() == reward.public_reference.as_deref() => {}
                    Ok(_) => return error_response(StatusCode::CONFLICT, "LEGACY_IMPORT_CONFLICT", "Legacy ticket reward request id conflicts with canonical state."),
                    Err(error) => {
                        tracing::error!(%error, request_id=%reward.request_id, "AREA legacy ticket reward reconciliation failed");
                        return temporary();
                    }
                }
            }
            Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("23505") => {
                return error_response(StatusCode::CONFLICT, "LEGACY_IMPORT_CONFLICT", "Legacy ticket reward conflicts with canonical state.");
            }
            Err(error) => {
                tracing::error!(%error, request_id=%reward.request_id, "AREA legacy ticket reward import failed");
                return temporary();
            }
        }
    }

    let source_balance = match i32::try_from(payload.token_balance) {
        Ok(value) => value,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Legacy balance is invalid."),
    };
    let source_voucher_count = match i32::try_from(payload.vouchers.len()) {
        Ok(value) => value,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Too many legacy vouchers."),
    };
    let source_ticket_reward_count = match i32::try_from(payload.ticket_rewards.len()) {
        Ok(value) => value,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Too many legacy ticket rewards."),
    };
    let marker = sqlx::query(
        r#"
        INSERT INTO area_legacy_wallet_imports (
            workspace_id, player_id, migration_id, source_balance,
            source_voucher_count, source_ticket_reward_count
        )
        VALUES ($1,$2,$3,$4,$5,$6)
        ON CONFLICT (workspace_id, player_id) DO NOTHING
        "#,
    )
    .bind(workspace_id)
    .bind(player_id)
    .bind(&payload.migration_id)
    .bind(source_balance)
    .bind(source_voucher_count)
    .bind(source_ticket_reward_count)
    .execute(&mut *tx)
    .await;
    match marker {
        Ok(result) if result.rows_affected() == 1 => {}
        Ok(_) => return error_response(StatusCode::CONFLICT, "MIGRATION_ALREADY_APPLIED", "Legacy wallet migration raced another request."),
        Err(error) => {
            tracing::error!(%error, "AREA legacy wallet marker insert failed");
            return temporary();
        }
    }
    let final_balance = match area_credit_balance_tx(&mut tx, workspace_id, player_id).await {
        Ok(value) => value,
        Err(_) => return temporary(),
    };
    if final_balance != target_balance {
        tracing::error!(player_id=%player_id, expected=target_balance, actual=final_balance, "AREA legacy wallet balance reconciliation failed");
        return temporary();
    }
    if let Err(error) = tx.commit().await {
        tracing::error!(%error, "AREA legacy wallet commit failed");
        return temporary();
    }
    crate::http_metrics().record_legacy_area_wallet_import();
    match wallet_for_player(&state, player_id).await {
        Ok(wallet) => (StatusCode::OK, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(wallet)).into_response(),
        Err(error) => {
            tracing::error!(%error, "AREA legacy wallet reload failed");
            temporary()
        }
    }
}
