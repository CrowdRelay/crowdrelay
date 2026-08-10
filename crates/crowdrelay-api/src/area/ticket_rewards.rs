#[derive(Clone, Debug, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
struct InternalTicketReward {
    request_id: Uuid,
    event_slug: String,
    credits: i32,
    fan_email: String,
    status: String,
    reservation_id: String,
    reservation_expires_at: i64,
    public_reference: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    issued_at: Option<OffsetDateTime>,
    failure_code: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TicketRewardReservationResponse {
    state: &'static str,
    reward: Option<InternalTicketReward>,
}

async fn ticket_reward_by_request_tx(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    player_id: Uuid,
    request_id: Uuid,
    lock: bool,
) -> Result<Option<InternalTicketReward>, sqlx::Error> {
    let suffix = if lock { " FOR UPDATE" } else { "" };
    let query = format!(
        r#"
        SELECT request_id, event_slug, credits, fan_email, status, reservation_id,
               floor(extract(epoch FROM reservation_expires_at) * 1000)::bigint AS reservation_expires_at,
               public_reference, issued_at, failure_code, created_at
        FROM area_ticket_rewards
        WHERE workspace_id = $1 AND player_id = $2 AND request_id = $3
        {suffix}
        "#,
    );
    sqlx::query_as::<_, InternalTicketReward>(&query)
        .bind(workspace_id)
        .bind(player_id)
        .bind(request_id)
        .fetch_optional(&mut **tx)
        .await
}

async fn active_ticket_reward_for_event_tx(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    player_id: Uuid,
    event_slug: &str,
) -> Result<Option<InternalTicketReward>, sqlx::Error> {
    sqlx::query_as::<_, InternalTicketReward>(
        r#"
        SELECT request_id, event_slug, credits, fan_email, status, reservation_id,
               floor(extract(epoch FROM reservation_expires_at) * 1000)::bigint AS reservation_expires_at,
               public_reference, issued_at, failure_code, created_at
        FROM area_ticket_rewards
        WHERE workspace_id = $1 AND player_id = $2 AND event_slug = $3
          AND status IN ('reserved', 'issued')
        ORDER BY created_at DESC
        LIMIT 1
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(player_id)
    .bind(event_slug)
    .fetch_optional(&mut **tx)
    .await
}

fn ticket_reservation_response(state: &'static str, reward: Option<InternalTicketReward>) -> Response {
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(TicketRewardReservationResponse { state, reward }),
    )
        .into_response()
}

pub async fn internal_ticket_rewards(
    State(state): State<crate::AppState>,
    Path(player_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Unauthorized.");
    }
    if !matches!(player_exists(&state, player_id).await, Ok(true)) {
        return error_response(StatusCode::NOT_FOUND, "NOT_FOUND", "Player not found.");
    }
    match load_ticket_rewards(&state, player_id).await {
        Ok(rewards) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(rewards),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "AREA ticket rewards list failed");
            temporary()
        }
    }
}

pub async fn internal_ticket_reward_reserve(
    State(state): State<crate::AppState>,
    Path(player_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<ReserveTicketRewardRequest>, JsonRejection>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) {
        return error_response(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Unauthorized.");
    }
    if !valid_idempotency_key(&headers) {
        return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "A valid Idempotency-Key is required.");
    }
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Invalid ticket reward request."),
    };
    let Some(email) = normalize_email(&payload.fan_email) else {
        return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Invalid ticket reward email.");
    };
    if !valid_event_slug(&payload.event_slug)
        || payload.credits == 0
        || payload.credits > 20
        || !valid_small_text(&payload.reservation_id, 128)
        || payload.reservation_expires_at <= epoch_millis(OffsetDateTime::now_utc()) as i64
    {
        return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Invalid ticket reward request.");
    }
    let credits = match i32::try_from(payload.credits) {
        Ok(value) => value,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Invalid ticket reward credits."),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(tx) => tx,
        Err(error) => {
            tracing::error!(%error, "AREA ticket reward transaction failed to start");
            return temporary();
        }
    };
    match lock_area_player(&mut tx, workspace_id, player_id).await {
        Ok(true) => {}
        Ok(false) => return error_response(StatusCode::NOT_FOUND, "NOT_FOUND", "Player not found."),
        Err(error) => {
            tracing::error!(%error, "AREA ticket reward player lock failed");
            return temporary();
        }
    }

    let existing_by_request = match ticket_reward_by_request_tx(
        &mut tx, workspace_id, player_id, payload.request_id, true,
    ).await {
        Ok(existing) => existing,
        Err(error) => {
            tracing::error!(%error, request_id=%payload.request_id, "AREA ticket reward idempotency lookup failed");
            return temporary();
        }
    };
    if let Some(existing) = existing_by_request {
        if existing.status == "issued" {
            if tx.commit().await.is_err() { return temporary(); }
            return ticket_reservation_response("issued", Some(existing));
        }
        if existing.status == "failed" {
            if tx.commit().await.is_err() { return temporary(); }
            return ticket_reservation_response("failed", Some(existing));
        }
        if existing.event_slug != payload.event_slug
            || existing.credits != credits
            || existing.fan_email != email
        {
            return error_response(StatusCode::CONFLICT, "REQUEST_MISMATCH", "Ticket reward request does not match its first use.");
        }
        if let Err(error) = sqlx::query(
            r#"
            UPDATE area_ticket_rewards
            SET fan_email = $4, reservation_id = $5,
                reservation_expires_at = to_timestamp($6::double precision / 1000.0),
                failure_code = NULL, updated_at = now()
            WHERE workspace_id = $1 AND player_id = $2 AND request_id = $3
            "#,
        )
        .bind(workspace_id).bind(player_id).bind(payload.request_id).bind(&email)
        .bind(&payload.reservation_id).bind(payload.reservation_expires_at)
        .execute(&mut *tx).await {
            tracing::error!(%error, "AREA ticket reward lease resume failed");
            return temporary();
        }
        let reward = match ticket_reward_by_request_tx(&mut tx, workspace_id, player_id, payload.request_id, false).await {
            Ok(Some(reward)) => reward,
            _ => return temporary(),
        };
        if tx.commit().await.is_err() { return temporary(); }
        return ticket_reservation_response("acquired", Some(reward));
    }

    let now_ms = epoch_millis(OffsetDateTime::now_utc()) as i64;
    match active_ticket_reward_for_event_tx(&mut tx, workspace_id, player_id, &payload.event_slug).await {
        Ok(Some(existing)) if existing.status == "issued" => {
            if tx.commit().await.is_err() { return temporary(); }
            return ticket_reservation_response("issued", Some(existing));
        }
        Ok(Some(existing)) if existing.reservation_expires_at > now_ms => {
            if tx.commit().await.is_err() { return temporary(); }
            return ticket_reservation_response("busy", Some(existing));
        }
        Ok(Some(stale)) => {
            if let Err(error) = sqlx::query(
                r#"UPDATE area_ticket_rewards SET status='failed', failure_code='lease_expired', updated_at=now() WHERE workspace_id=$1 AND player_id=$2 AND request_id=$3 AND status='reserved'"#,
            ).bind(workspace_id).bind(player_id).bind(stale.request_id).execute(&mut *tx).await {
                tracing::error!(%error, "AREA stale ticket reward close failed");
                return temporary();
            }
            let refund_reference = format!("ticket-refund:{}", stale.request_id);
            if let Err(error) = insert_credit_delta(
                &mut tx, workspace_id, player_id, stale.credits, "ticket_reward_refund",
                &refund_reference, None,
            ).await {
                tracing::error!(%error, "AREA stale ticket reward refund failed");
                return temporary();
            }
        }
        Ok(None) => {}
        Err(error) => {
            tracing::error!(%error, "AREA ticket reward event lookup failed");
            return temporary();
        }
    }

    let balance = match area_credit_balance_tx(&mut tx, workspace_id, player_id).await {
        Ok(balance) => balance,
        Err(error) => { tracing::error!(%error, "AREA ticket reward balance failed"); return temporary(); }
    };
    if balance < i64::from(credits) {
        if tx.commit().await.is_err() { return temporary(); }
        return ticket_reservation_response("insufficient", None);
    }
    if let Err(error) = sqlx::query(
        r#"
        INSERT INTO area_ticket_rewards (
            workspace_id, player_id, request_id, event_slug, credits, fan_email,
            status, reservation_id, reservation_expires_at
        )
        VALUES ($1,$2,$3,$4,$5,$6,'reserved',$7,to_timestamp($8::double precision / 1000.0))
        "#,
    )
    .bind(workspace_id).bind(player_id).bind(payload.request_id).bind(&payload.event_slug)
    .bind(credits).bind(&email).bind(&payload.reservation_id).bind(payload.reservation_expires_at)
    .execute(&mut *tx).await {
        tracing::error!(%error, "AREA ticket reward reservation insert failed");
        return temporary();
    }
    let debit_reference = format!("ticket:{}", payload.request_id);
    if let Err(error) = insert_credit_delta(
        &mut tx, workspace_id, player_id, -credits, "ticket_reward_spend", &debit_reference, None,
    ).await {
        tracing::error!(%error, "AREA ticket reward debit failed");
        return temporary();
    }
    let reward = match ticket_reward_by_request_tx(&mut tx, workspace_id, player_id, payload.request_id, false).await {
        Ok(Some(reward)) => reward,
        _ => return temporary(),
    };
    if tx.commit().await.is_err() { return temporary(); }
    ticket_reservation_response("acquired", Some(reward))
}

pub async fn internal_ticket_reward_finalize(
    State(state): State<crate::AppState>,
    Path(player_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<FinalizeTicketRewardRequest>, JsonRejection>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) { return error_response(StatusCode::UNAUTHORIZED,"UNAUTHORIZED","Unauthorized."); }
    if !valid_idempotency_key(&headers) { return error_response(StatusCode::BAD_REQUEST,"INVALID_REQUEST","A valid Idempotency-Key is required."); }
    let Json(payload)=match payload { Ok(v)=>v,Err(_)=>return error_response(StatusCode::BAD_REQUEST,"INVALID_REQUEST","Invalid ticket reward finalization.") };
    if !valid_small_text(&payload.reservation_id,128) || !valid_small_text(&payload.public_reference,128) {
        return error_response(StatusCode::BAD_REQUEST,"INVALID_REQUEST","Invalid ticket reward finalization.");
    }
    let workspace_id=state.ticketing.workspace_id().into_uuid();
    let mut tx=match state.ticketing.pool().begin().await { Ok(tx)=>tx,Err(_)=>return temporary() };
    let Some(existing)=(match ticket_reward_by_request_tx(&mut tx,workspace_id,player_id,payload.request_id,true).await { Ok(r)=>r,Err(_)=>return temporary() }) else {
        return error_response(StatusCode::NOT_FOUND,"NOT_FOUND","Ticket reward not found.");
    };
    if existing.status=="issued" {
        if tx.commit().await.is_err(){return temporary();}
        return ticket_reservation_response("issued",Some(existing));
    }
    if existing.status!="reserved" || existing.reservation_id!=payload.reservation_id {
        return error_response(StatusCode::CONFLICT,"REWARD_MISMATCH","Ticket reward reservation ownership was lost.");
    }
    if let Err(error)=sqlx::query(r#"UPDATE area_ticket_rewards SET status='issued',public_reference=$4,issued_at=now(),failure_code=NULL,updated_at=now() WHERE workspace_id=$1 AND player_id=$2 AND request_id=$3"#)
        .bind(workspace_id).bind(player_id).bind(payload.request_id).bind(&payload.public_reference).execute(&mut *tx).await {
        tracing::error!(%error,"AREA ticket reward finalization failed");return temporary();
    }
    let reward=match ticket_reward_by_request_tx(&mut tx,workspace_id,player_id,payload.request_id,false).await { Ok(Some(r))=>r,_=>return temporary() };
    if tx.commit().await.is_err(){return temporary();}
    ticket_reservation_response("issued",Some(reward))
}

pub async fn internal_ticket_reward_fail(
    State(state): State<crate::AppState>,
    Path(player_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<FailTicketRewardRequest>, JsonRejection>,
) -> Response {
    if !state.ticketing.commerce_authorized(&headers) { return error_response(StatusCode::UNAUTHORIZED,"UNAUTHORIZED","Unauthorized."); }
    if !valid_idempotency_key(&headers) { return error_response(StatusCode::BAD_REQUEST,"INVALID_REQUEST","A valid Idempotency-Key is required."); }
    let Json(payload)=match payload { Ok(v)=>v,Err(_)=>return error_response(StatusCode::BAD_REQUEST,"INVALID_REQUEST","Invalid ticket reward failure.") };
    if !valid_small_text(&payload.reservation_id, 128)
        || payload.failure_code.as_deref().is_some_and(|value| !valid_small_text(value, 128))
    {
        return error_response(StatusCode::BAD_REQUEST,"INVALID_REQUEST","Invalid ticket reward failure.");
    }
    let workspace_id=state.ticketing.workspace_id().into_uuid();
    let mut tx=match state.ticketing.pool().begin().await { Ok(tx)=>tx,Err(_)=>return temporary() };
    if !matches!(lock_area_player(&mut tx,workspace_id,player_id).await,Ok(true)) { return error_response(StatusCode::NOT_FOUND,"NOT_FOUND","Player not found."); }
    let Some(existing)=(match ticket_reward_by_request_tx(&mut tx,workspace_id,player_id,payload.request_id,true).await { Ok(r)=>r,Err(_)=>return temporary() }) else {
        return error_response(StatusCode::NOT_FOUND,"NOT_FOUND","Ticket reward not found.");
    };
    if existing.status=="issued" {
        if tx.commit().await.is_err(){return temporary();}
        return ticket_reservation_response("issued",Some(existing));
    }
    if existing.status=="failed" {
        if tx.commit().await.is_err(){return temporary();}
        return ticket_reservation_response("failed",Some(existing));
    }
    if existing.reservation_id!=payload.reservation_id {
        return error_response(StatusCode::CONFLICT,"REWARD_MISMATCH","Ticket reward reservation ownership was lost.");
    }
    if payload.permanent {
        if let Err(error)=sqlx::query(r#"UPDATE area_ticket_rewards SET status='failed',failure_code=$4,updated_at=now() WHERE workspace_id=$1 AND player_id=$2 AND request_id=$3"#)
            .bind(workspace_id).bind(player_id).bind(payload.request_id).bind(payload.failure_code.as_deref()).execute(&mut *tx).await {
            tracing::error!(%error,"AREA ticket reward permanent failure update failed");return temporary();
        }
        let refund_reference=format!("ticket-refund:{}",payload.request_id);
        if let Err(error)=insert_credit_delta(&mut tx,workspace_id,player_id,existing.credits,"ticket_reward_refund",&refund_reference,None).await {
            tracing::error!(%error,"AREA ticket reward refund failed");return temporary();
        }
    } else if let Err(error)=sqlx::query(r#"UPDATE area_ticket_rewards SET reservation_expires_at=GREATEST(created_at + interval '1 second', now()),failure_code=NULL,updated_at=now() WHERE workspace_id=$1 AND player_id=$2 AND request_id=$3"#)
        .bind(workspace_id).bind(player_id).bind(payload.request_id).execute(&mut *tx).await {
        tracing::error!(%error,"AREA ticket reward retry release failed");return temporary();
    }
    let reward=match ticket_reward_by_request_tx(&mut tx,workspace_id,player_id,payload.request_id,false).await { Ok(Some(r))=>r,_=>return temporary() };
    let state_name=if payload.permanent {"failed"} else {"acquired"};
    if tx.commit().await.is_err(){return temporary();}
    ticket_reservation_response(state_name,Some(reward))
}
