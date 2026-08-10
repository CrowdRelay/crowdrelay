#[derive(Clone, Debug, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
struct AreaRewardRecord {
    version: i32,
    code_hash: String,
    code_suffix: String,
    owner_id: Uuid,
    owner_hash: String,
    request_id: Uuid,
    benefit: String,
    status: String,
    #[serde(with = "time::serde::rfc3339")]
    issued_at: OffsetDateTime,
    expires_at: i64,
    reservation_id: Option<String>,
    reserved_until: Option<i64>,
    checkout_session_id: Option<String>,
    free_product_id: Option<String>,
    free_product_label: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    redeemed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RewardPreviewResponse {
    valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    benefit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<i64>,
    resumed: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RewardReserveResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    record: Option<AreaRewardRecord>,
    resumed: bool,
}

fn owner_hash(player_id: Uuid) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"virya-area-owner\0");
    hasher.update(player_id.to_string().as_bytes());
    hex::encode(hasher.finalize())
}

async fn reward_record_by_hash_tx(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    code_hash: &[u8],
    lock: bool,
) -> Result<Option<AreaRewardRecord>, sqlx::Error> {
    let lock_clause = if lock { " FOR UPDATE" } else { "" };
    let query = format!(
        r#"
        SELECT
            1::integer AS version,
            encode(code_hash, 'hex') AS code_hash,
            code_suffix,
            player_id AS owner_id,
            $3::text AS owner_hash,
            request_id,
            benefit,
            status,
            issued_at,
            floor(extract(epoch FROM expires_at))::bigint AS expires_at,
            reservation_id,
            CASE WHEN reserved_until IS NULL THEN NULL
                 ELSE floor(extract(epoch FROM reserved_until) * 1000)::bigint END AS reserved_until,
            checkout_session_id,
            free_product_id,
            free_product_label,
            redeemed_at,
            updated_at
        FROM area_reward_vouchers
        WHERE workspace_id = $1 AND code_hash = $2
        {lock_clause}
        "#,
    );
    let player_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT player_id FROM area_reward_vouchers WHERE workspace_id = $1 AND code_hash = $2",
    )
    .bind(workspace_id)
    .bind(code_hash)
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(player_id) = player_id else {
        return Ok(None);
    };
    sqlx::query_as::<_, AreaRewardRecord>(&query)
        .bind(workspace_id)
        .bind(code_hash)
        .bind(owner_hash(player_id))
        .fetch_optional(&mut **transaction)
        .await
}

async fn reward_public_by_request_tx(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    player_id: Uuid,
    request_id: Uuid,
) -> Result<Option<AreaVoucherPublic>, sqlx::Error> {
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
        WHERE workspace_id = $1 AND player_id = $2 AND request_id = $3
        "#,
    )
    .bind(workspace_id)
    .bind(player_id)
    .bind(request_id)
    .fetch_optional(&mut **transaction)
    .await
}

fn reward_hash_bytes(raw: &str) -> Option<Vec<u8>> {
    (raw.len() == 64 && raw.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| hex::decode(raw).ok())
        .flatten()
        .filter(|bytes| bytes.len() == 32)
}

fn reward_failure(reason: &'static str) -> Response {
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(RewardReserveResponse {
            ok: false,
            reason: Some(reason),
            record: None,
            resumed: false,
        }),
    )
        .into_response()
}

pub async fn internal_create_voucher(
    State(state): State<crate::AppState>,
    Path(player_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<CreateVoucherRequest>, JsonRejection>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Unauthorized.");
    }
    if !valid_idempotency_key(&headers) {
        return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "A valid Idempotency-Key is required.");
    }
    let Json(payload) = match payload {
        Ok(payload) if payload.tokens == 1 => payload,
        _ => return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Invalid reward request."),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = match state.ticketing.pool().begin().await {
        Ok(transaction) => transaction,
        Err(error) => {
            tracing::error!(%error, "AREA voucher transaction failed to start");
            return temporary();
        }
    };
    match lock_area_player(&mut transaction, workspace_id, player_id).await {
        Ok(true) => {}
        Ok(false) => return error_response(StatusCode::NOT_FOUND, "NOT_FOUND", "Player not found."),
        Err(error) => {
            tracing::error!(%error, "AREA voucher player lock failed");
            return temporary();
        }
    }
    match reward_public_by_request_tx(&mut transaction, workspace_id, player_id, payload.request_id).await {
        Ok(Some(existing)) => {
            if transaction.commit().await.is_err() { return temporary(); }
            return (StatusCode::OK, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(existing)).into_response();
        }
        Ok(None) => {}
        Err(error) => {
            tracing::error!(%error, "AREA voucher idempotency lookup failed");
            return temporary();
        }
    }
    let balance = match area_credit_balance_tx(&mut transaction, workspace_id, player_id).await {
        Ok(balance) => balance,
        Err(error) => {
            tracing::error!(%error, "AREA voucher balance lookup failed");
            return temporary();
        }
    };
    if balance < 1 {
        return error_response(StatusCode::CONFLICT, "INSUFFICIENT_CREDITS", "Not enough VIRYA Credits.");
    }

    let issued_at = OffsetDateTime::now_utc();
    let expires_at = issued_at + Duration::days(365);
    let mut inserted = None;
    for _ in 0..4 {
        let code = match new_reward_code() {
            Some(code) => code,
            None => {
                tracing::error!("AREA voucher entropy unavailable");
                return temporary();
            }
        };
        let hash = area_reward_code_hash(&code);
        let result = sqlx::query(
            r#"
            INSERT INTO area_reward_vouchers (
                workspace_id, player_id, request_id, code, code_hash, code_suffix,
                token_cost, benefit, status, issued_at, expires_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, 1, 'free-item-and-shipping', 'issued', $7, $8)
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(workspace_id)
        .bind(player_id)
        .bind(payload.request_id)
        .bind(&code)
        .bind(hash)
        .bind(code.get(code.len().saturating_sub(4)..).unwrap_or_default())
        .bind(issued_at)
        .bind(expires_at)
        .execute(&mut *transaction)
        .await;
        match result {
            Ok(result) if result.rows_affected() == 1 => {
                inserted = Some(code);
                break;
            }
            Ok(_) => {
                if let Ok(Some(existing)) = reward_public_by_request_tx(
                    &mut transaction,
                    workspace_id,
                    player_id,
                    payload.request_id,
                )
                .await
                {
                    if transaction.commit().await.is_err() { return temporary(); }
                    return (StatusCode::OK, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(existing)).into_response();
                }
            }
            Err(error) => {
                tracing::error!(%error, "AREA voucher insert failed");
                return temporary();
            }
        }
    }
    if inserted.is_none() {
        tracing::error!("AREA voucher code collision budget exhausted");
        return temporary();
    }
    let reference = format!("voucher:{}", payload.request_id);
    match insert_credit_delta(
        &mut transaction,
        workspace_id,
        player_id,
        -1,
        "voucher_spend",
        &reference,
        Some(issued_at),
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {}
        Err(error) => {
            tracing::error!(%error, "AREA voucher debit failed");
            return temporary();
        }
    }
    let voucher = match reward_public_by_request_tx(
        &mut transaction,
        workspace_id,
        player_id,
        payload.request_id,
    )
    .await
    {
        Ok(Some(voucher)) => voucher,
        Ok(None) => return temporary(),
        Err(error) => {
            tracing::error!(%error, "AREA voucher reload failed");
            return temporary();
        }
    };
    if let Err(error) = transaction.commit().await {
        tracing::error!(%error, "AREA voucher commit failed");
        return temporary();
    }
    (StatusCode::OK, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(voucher)).into_response()
}

pub async fn internal_reward_preview(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<RewardCodeRequest>, JsonRejection>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Unauthorized.");
    }
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Invalid reward request."),
    };
    let Some(code) = normalize_reward_code(&payload.code) else {
        return (StatusCode::OK, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(RewardPreviewResponse {
            valid: false, reason: Some("invalid"), code: None, code_hash: None,
            benefit: None, expires_at: None, resumed: false,
        })).into_response();
    };
    let hash = area_reward_code_hash(&code);
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = match state.ticketing.pool().begin().await { Ok(tx) => tx, Err(_) => return temporary() };
    let record = match reward_record_by_hash_tx(&mut transaction, workspace_id, &hash, false).await {
        Ok(record) => record,
        Err(error) => { tracing::error!(%error, "AREA reward preview failed"); return temporary(); }
    };
    if transaction.commit().await.is_err() { return temporary(); }
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let response = match record {
        None => RewardPreviewResponse { valid:false, reason:Some("invalid"), code:None, code_hash:None, benefit:None, expires_at:None, resumed:false },
        Some(record) if record.expires_at <= now => RewardPreviewResponse { valid:false, reason:Some("expired"), code:None, code_hash:None, benefit:None, expires_at:None, resumed:false },
        Some(record) if record.status == "redeemed" => RewardPreviewResponse { valid:false, reason:Some("redeemed"), code:None, code_hash:None, benefit:None, expires_at:None, resumed:false },
        Some(record) if record.status == "reserved"
            && record.reserved_until.is_some_and(|until| until > epoch_millis(OffsetDateTime::now_utc()) as i64)
            && payload.reservation_id.as_deref() != record.reservation_id.as_deref() =>
            RewardPreviewResponse { valid:false, reason:Some("busy"), code:None, code_hash:None, benefit:None, expires_at:None, resumed:false },
        Some(record) => RewardPreviewResponse {
            valid:true, reason:None, code:Some(code), code_hash:Some(record.code_hash.clone()),
            benefit:Some(record.benefit.clone()), expires_at:Some(record.expires_at),
            resumed: record.status == "reserved" && payload.reservation_id.as_deref() == record.reservation_id.as_deref(),
        },
    };
    (StatusCode::OK, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(response)).into_response()
}

pub async fn internal_reward_reserve(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ReserveRewardRequest>, JsonRejection>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Unauthorized.");
    }
    if !valid_idempotency_key(&headers) {
        return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "A valid Idempotency-Key is required.");
    }
    let Json(payload) = match payload { Ok(value) => value, Err(_) => return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Invalid reward request.") };
    if !valid_small_text(&payload.reservation_id, 128) || payload.reserved_until <= epoch_millis(OffsetDateTime::now_utc()) as i64 {
        return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Invalid reward reservation.");
    }
    let Some(code) = normalize_reward_code(&payload.code) else { return reward_failure("invalid"); };
    let hash = area_reward_code_hash(&code);
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await { Ok(tx) => tx, Err(_) => return temporary() };
    let record = match reward_record_by_hash_tx(&mut tx, workspace_id, &hash, true).await {
        Ok(Some(record)) => record,
        Ok(None) => return reward_failure("invalid"),
        Err(error) => { tracing::error!(%error, "AREA reward reservation lookup failed"); return temporary(); }
    };
    let now_sec = OffsetDateTime::now_utc().unix_timestamp();
    let now_ms = epoch_millis(OffsetDateTime::now_utc()) as i64;
    if record.expires_at <= now_sec { return reward_failure("expired"); }
    if record.status == "redeemed" { return reward_failure("redeemed"); }
    if record.status == "reserved" && record.reservation_id.as_deref() == Some(payload.reservation_id.as_str()) {
        if tx.commit().await.is_err() { return temporary(); }
        return (StatusCode::OK, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(RewardReserveResponse { ok:true, reason:None, record:Some(record), resumed:true })).into_response();
    }
    if record.status == "reserved" && record.reserved_until.is_some_and(|until| until > now_ms) {
        return reward_failure("busy");
    }
    let updated = sqlx::query(
        r#"
        UPDATE area_reward_vouchers
        SET status = 'reserved', reservation_id = $3,
            reserved_until = to_timestamp($4::double precision / 1000.0),
            checkout_session_id = NULL, free_product_id = NULL, free_product_label = NULL,
            updated_at = now()
        WHERE workspace_id = $1 AND code_hash = $2
        "#,
    )
    .bind(workspace_id).bind(&hash).bind(&payload.reservation_id).bind(payload.reserved_until)
    .execute(&mut *tx).await;
    if let Err(error) = updated { tracing::error!(%error, "AREA reward reservation update failed"); return temporary(); }
    let record = match reward_record_by_hash_tx(&mut tx, workspace_id, &hash, false).await { Ok(Some(r)) => r, _ => return temporary() };
    if tx.commit().await.is_err() { return temporary(); }
    (StatusCode::OK, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(RewardReserveResponse { ok:true, reason:None, record:Some(record), resumed:false })).into_response()
}

pub async fn internal_reward_attach(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<AttachRewardCheckoutRequest>, JsonRejection>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) { return error_response(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Unauthorized."); }
    if !valid_idempotency_key(&headers) { return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "A valid Idempotency-Key is required."); }
    let Json(payload) = match payload { Ok(value) => value, Err(_) => return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Invalid reward request.") };
    let Some(hash) = reward_hash_bytes(&payload.code_hash) else { return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Invalid reward hash."); };
    if !valid_small_text(&payload.reservation_id,128) || !valid_small_text(&payload.checkout_session_id,255) || !valid_small_text(&payload.free_product_id,128) || !valid_small_text(&payload.free_product_label,256) {
        return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Invalid reward attachment.");
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await { Ok(tx) => tx, Err(_) => return temporary() };
    let Some(record) = (match reward_record_by_hash_tx(&mut tx, workspace_id, &hash, true).await { Ok(r) => r, Err(_) => return temporary() }) else {
        return error_response(StatusCode::NOT_FOUND, "NOT_FOUND", "Reward not found.");
    };
    if record.status != "reserved" || record.reservation_id.as_deref() != Some(payload.reservation_id.as_str()) {
        return error_response(StatusCode::CONFLICT, "REWARD_MISMATCH", "Reward reservation ownership was lost.");
    }
    if record.checkout_session_id.as_deref().is_some_and(|id| id != payload.checkout_session_id) {
        return error_response(StatusCode::CONFLICT, "REWARD_MISMATCH", "Reward is attached to another checkout.");
    }
    if let Err(error) = sqlx::query(
        r#"UPDATE area_reward_vouchers SET checkout_session_id=$3, free_product_id=$4, free_product_label=$5, updated_at=now() WHERE workspace_id=$1 AND code_hash=$2"#,
    ).bind(workspace_id).bind(&hash).bind(&payload.checkout_session_id).bind(&payload.free_product_id).bind(&payload.free_product_label).execute(&mut *tx).await {
        tracing::error!(%error, "AREA reward checkout attach failed"); return temporary();
    }
    let record = match reward_record_by_hash_tx(&mut tx, workspace_id, &hash, false).await { Ok(Some(r)) => r, _ => return temporary() };
    if tx.commit().await.is_err() { return temporary(); }
    (StatusCode::OK, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(record)).into_response()
}

pub async fn internal_reward_redeem(
    State(state): State<crate::AppState>, headers: HeaderMap,
    payload: Result<Json<ReconcileRewardRequest>, JsonRejection>,
) -> Response {
    reconcile_reward(state, headers, payload, true).await
}

pub async fn internal_reward_release(
    State(state): State<crate::AppState>, headers: HeaderMap,
    payload: Result<Json<ReleaseRewardRequest>, JsonRejection>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) { return error_response(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Unauthorized."); }
    if !valid_idempotency_key(&headers) { return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "A valid Idempotency-Key is required."); }
    let Json(payload) = match payload { Ok(value) => value, Err(_) => return error_response(StatusCode::BAD_REQUEST,"INVALID_REQUEST","Invalid reward request.") };
    let Some(hash) = reward_hash_bytes(&payload.code_hash) else { return error_response(StatusCode::BAD_REQUEST,"INVALID_REQUEST","Invalid reward hash."); };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await { Ok(tx)=>tx, Err(_)=>return temporary() };
    let Some(record) = (match reward_record_by_hash_tx(&mut tx, workspace_id, &hash, true).await { Ok(r)=>r, Err(_)=>return temporary() }) else {
        if tx.commit().await.is_err() { return temporary(); }
        return (StatusCode::OK, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(Option::<AreaRewardRecord>::None)).into_response();
    };
    if record.status == "redeemed" || record.status != "reserved" || record.reservation_id.as_deref() != Some(payload.reservation_id.as_str()) {
        if tx.commit().await.is_err() { return temporary(); }
        return (StatusCode::OK, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(Option::<AreaRewardRecord>::None)).into_response();
    }
    if let Some(expected) = payload.checkout_session_id.as_deref()
        && record
            .checkout_session_id
            .as_deref()
            .is_some_and(|actual| actual != expected)
    {
            if tx.commit().await.is_err() { return temporary(); }
            return (StatusCode::OK, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(Option::<AreaRewardRecord>::None)).into_response();
    }
    if let Err(error) = sqlx::query(r#"UPDATE area_reward_vouchers SET status='issued', reservation_id=NULL, reserved_until=NULL, checkout_session_id=NULL, free_product_id=NULL, free_product_label=NULL, updated_at=now() WHERE workspace_id=$1 AND code_hash=$2"#)
        .bind(workspace_id).bind(&hash).execute(&mut *tx).await { tracing::error!(%error,"AREA reward release failed"); return temporary(); }
    let record = match reward_record_by_hash_tx(&mut tx, workspace_id, &hash, false).await { Ok(Some(r))=>r, _=>return temporary() };
    if tx.commit().await.is_err() { return temporary(); }
    (StatusCode::OK, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(Some(record))).into_response()
}

async fn reconcile_reward(
    state: crate::AppState,
    headers: HeaderMap,
    payload: Result<Json<ReconcileRewardRequest>, JsonRejection>,
    redeem: bool,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) { return error_response(StatusCode::UNAUTHORIZED,"UNAUTHORIZED","Unauthorized."); }
    if !valid_idempotency_key(&headers) { return error_response(StatusCode::BAD_REQUEST,"INVALID_REQUEST","A valid Idempotency-Key is required."); }
    let Json(payload)=match payload { Ok(v)=>v, Err(_)=>return error_response(StatusCode::BAD_REQUEST,"INVALID_REQUEST","Invalid reward request.") };
    let Some(hash)=reward_hash_bytes(&payload.code_hash) else { return error_response(StatusCode::BAD_REQUEST,"INVALID_REQUEST","Invalid reward hash."); };
    let workspace_id=state.ticketing.workspace_id().into_uuid();
    let mut tx=match state.ticketing.pool().begin().await { Ok(tx)=>tx,Err(_)=>return temporary() };
    let Some(record)=(match reward_record_by_hash_tx(&mut tx,workspace_id,&hash,true).await { Ok(r)=>r,Err(_)=>return temporary() }) else {
        if tx.commit().await.is_err(){return temporary();}
        return (StatusCode::OK,[(CACHE_CONTROL,PRIVATE_NO_STORE)],Json(Option::<AreaRewardRecord>::None)).into_response();
    };
    if record.status=="redeemed" && record.checkout_session_id.as_deref()==Some(payload.checkout_session_id.as_str()) {
        if tx.commit().await.is_err(){return temporary();}
        return (StatusCode::OK,[(CACHE_CONTROL,PRIVATE_NO_STORE)],Json(Some(record))).into_response();
    }
    if !redeem || record.status!="reserved" || record.reservation_id.as_deref()!=Some(payload.reservation_id.as_str()) || record.checkout_session_id.as_deref()!=Some(payload.checkout_session_id.as_str()) {
        if tx.commit().await.is_err(){return temporary();}
        return (StatusCode::OK,[(CACHE_CONTROL,PRIVATE_NO_STORE)],Json(Option::<AreaRewardRecord>::None)).into_response();
    }
    if let Err(error)=sqlx::query(r#"UPDATE area_reward_vouchers SET status='redeemed', redeemed_at=now(), updated_at=now() WHERE workspace_id=$1 AND code_hash=$2"#)
        .bind(workspace_id).bind(&hash).execute(&mut *tx).await { tracing::error!(%error,"AREA reward redeem failed"); return temporary(); }
    let record=match reward_record_by_hash_tx(&mut tx,workspace_id,&hash,false).await { Ok(Some(r))=>r,_=>return temporary() };
    if tx.commit().await.is_err(){return temporary();}
    (StatusCode::OK,[(CACHE_CONTROL,PRIVATE_NO_STORE)],Json(Some(record))).into_response()
}
