pub async fn start_run(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<StartRunRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return SynesthesiaError::Invalid.response(request_id_value),
    };
    if validate_start(&payload).is_err() {
        return SynesthesiaError::Invalid.response(request_id_value);
    }

    let token = match random_token() {
        Ok(token) => token,
        Err(()) => return SynesthesiaError::Unavailable.response(request_id_value),
    };
    let token_hash = Sha256::digest(token.as_bytes()).to_vec();
    let install_hash =
        Sha256::digest(format!("{}\0{}", payload.campaign_slug, payload.install_id).as_bytes())
            .to_vec();
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let row = sqlx::query_as::<_, (Uuid, i16)>(
        r#"
        INSERT INTO synesthesia_runs (
            workspace_id, campaign_slug, install_hash, run_token_hash,
            app_version, attempt_id, locale
        ) VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (workspace_id, campaign_slug, install_hash, attempt_id) DO UPDATE
        SET run_token_hash = EXCLUDED.run_token_hash,
            app_version = EXCLUDED.app_version,
            locale = EXCLUDED.locale,
            updated_at = now()
        RETURNING id, next_room_index
        "#,
    )
    .bind(workspace_id)
    .bind(&payload.campaign_slug)
    .bind(install_hash)
    .bind(token_hash)
    .bind(payload.app_version.trim())
    .bind(clean_attempt_id(payload.attempt_id.as_deref()).unwrap_or_else(|| "legacy".to_owned()))
    .bind(clean_locale(payload.locale.as_deref()))
    .fetch_one(state.ticketing.pool())
    .await;

    match row {
        Ok((run_id, next_room_index)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
            Json(StartRunResponse {
                run_id,
                run_token: token,
                next_room_index,
            }),
        )
            .into_response(),
        Err(error) => SynesthesiaError::sqlx(error).response(request_id_value),
    }
}

pub async fn record_room(
    State(state): State<crate::AppState>,
    Path((run_id, room_id)): Path<(Uuid, String)>,
    headers: HeaderMap,
    payload: Result<Json<RecordRoomRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return SynesthesiaError::Invalid.response(request_id_value),
    };
    if payload.room_index < 0
        || usize::try_from(payload.room_index)
            .ok()
            .is_none_or(|index| index >= ROOM_IDS.len())
        || !(MIN_ROOM_ELAPSED_MS..=MAX_ROOM_ELAPSED_MS).contains(&payload.client_elapsed_ms)
    {
        return SynesthesiaError::Invalid.response(request_id_value);
    }
    let token_hash = match bearer_hash(&headers) {
        Some(hash) => hash,
        None => return SynesthesiaError::Unauthorized.response(request_id_value),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = match state.ticketing.pool().begin().await {
        Ok(transaction) => transaction,
        Err(error) => return SynesthesiaError::sqlx(error).response(request_id_value),
    };

    let result = record_room_inner(
        &mut transaction,
        workspace_id,
        run_id,
        &room_id,
        &token_hash,
        &payload,
    )
    .await;
    let next_room_index = match result {
        Ok(index) => index,
        Err(error) => return error.response(request_id_value),
    };
    if let Err(error) = transaction.commit().await {
        return SynesthesiaError::sqlx(error).response(request_id_value);
    }
    (
        StatusCode::OK,
        [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
        Json(RecordRoomResponse { next_room_index }),
    )
        .into_response()
}

async fn record_room_inner(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    run_id: Uuid,
    room_id: &str,
    token_hash: &[u8],
    payload: &RecordRoomRequest,
) -> Result<i16, SynesthesiaError> {
    let Some((campaign_slug, next_room_index, completed_at)) =
        sqlx::query_as::<_, (String, i16, Option<time::OffsetDateTime>)>(
            r#"
            SELECT campaign_slug, next_room_index, completed_at
            FROM synesthesia_runs
            WHERE workspace_id = $1 AND id = $2 AND run_token_hash = $3
            FOR UPDATE
            "#,
        )
        .bind(workspace_id)
        .bind(run_id)
        .bind(token_hash)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(SynesthesiaError::sqlx)?
    else {
        return Err(SynesthesiaError::Unauthorized);
    };
    if campaign_slug != CAMPAIGN_SLUG || completed_at.is_some() {
        return Err(SynesthesiaError::Conflict);
    }

    if payload.room_index < next_room_index {
        let exists = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM synesthesia_room_completions
                WHERE workspace_id = $1 AND run_id = $2
                  AND room_index = $3 AND room_id = $4
            )
            "#,
        )
        .bind(workspace_id)
        .bind(run_id)
        .bind(payload.room_index)
        .bind(room_id)
        .fetch_one(&mut **transaction)
        .await
        .map_err(SynesthesiaError::sqlx)?;
        return if exists {
            Ok(next_room_index)
        } else {
            Err(SynesthesiaError::Conflict)
        };
    }
    if payload.room_index != next_room_index {
        return Err(SynesthesiaError::Conflict);
    }
    let expected_room = usize::try_from(next_room_index)
        .ok()
        .and_then(|index| ROOM_IDS.get(index))
        .copied()
        .ok_or(SynesthesiaError::Conflict)?;
    if room_id != expected_room {
        return Err(SynesthesiaError::Conflict);
    }

    sqlx::query(
        r#"
        INSERT INTO synesthesia_room_completions (
            workspace_id, run_id, room_index, room_id, client_elapsed_ms
        ) VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (workspace_id, run_id, room_index) DO NOTHING
        "#,
    )
    .bind(workspace_id)
    .bind(run_id)
    .bind(payload.room_index)
    .bind(room_id)
    .bind(payload.client_elapsed_ms)
    .execute(&mut **transaction)
    .await
    .map_err(SynesthesiaError::sqlx)?;

    let advanced = next_room_index.saturating_add(1);
    sqlx::query(
        r#"
        UPDATE synesthesia_runs
        SET next_room_index = $3, updated_at = now()
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(run_id)
    .bind(advanced)
    .execute(&mut **transaction)
    .await
    .map_err(SynesthesiaError::sqlx)?;
    Ok(advanced)
}

pub async fn complete_run(
    State(state): State<crate::AppState>,
    Path(run_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<CompleteRunRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return SynesthesiaError::Invalid.response(request_id_value),
    };
    if !(i64::try_from(ROOM_IDS.len()).unwrap_or(11) * MIN_ROOM_ELAPSED_MS..=MAX_TOTAL_ELAPSED_MS)
        .contains(&payload.client_total_elapsed_ms)
    {
        return SynesthesiaError::Invalid.response(request_id_value);
    }
    let token_hash = match bearer_hash(&headers) {
        Some(hash) => hash,
        None => return SynesthesiaError::Unauthorized.response(request_id_value),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let row = sqlx::query_as::<_, (i16, Option<time::OffsetDateTime>, i64)>(
        r#"
        SELECT run.next_room_index, run.completed_at,
               COALESCE(SUM(room.client_elapsed_ms), 0)::bigint AS recorded_elapsed_ms
        FROM synesthesia_runs AS run
        LEFT JOIN synesthesia_room_completions AS room
          ON room.workspace_id = run.workspace_id AND room.run_id = run.id
        WHERE run.workspace_id = $1 AND run.id = $2 AND run.run_token_hash = $3
        GROUP BY run.id
        "#,
    )
    .bind(workspace_id)
    .bind(run_id)
    .bind(token_hash)
    .fetch_optional(state.ticketing.pool())
    .await;
    let Some((next_room_index, completed_at, recorded_elapsed_ms)) = (match row {
        Ok(row) => row,
        Err(error) => return SynesthesiaError::sqlx(error).response(request_id_value),
    }) else {
        return SynesthesiaError::Unauthorized.response(request_id_value);
    };
    if completed_at.is_none() {
        if usize::try_from(next_room_index).ok() != Some(ROOM_IDS.len())
            || payload.client_total_elapsed_ms != recorded_elapsed_ms
        {
            return SynesthesiaError::Conflict.response(request_id_value);
        }
        let result = sqlx::query(
            r#"
            UPDATE synesthesia_runs
            SET completed_at = now(), client_total_elapsed_ms = $3, updated_at = now()
            WHERE workspace_id = $1 AND id = $2 AND completed_at IS NULL
            "#,
        )
        .bind(workspace_id)
        .bind(run_id)
        .bind(payload.client_total_elapsed_ms)
        .execute(state.ticketing.pool())
        .await;
        if let Err(error) = result {
            return SynesthesiaError::sqlx(error).response(request_id_value);
        }
    }

    match completion_response(&state, workspace_id, run_id, true).await {
        Ok(response) => (
            StatusCode::OK,
            [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
            Json(response),
        )
            .into_response(),
        Err(error) => error.response(request_id_value),
    }
}

/// Mark a locally complete legacy save as non-competitive.
///
/// This compatibility path exists only for builds affected by the historical
/// room-timing capture bug. It never writes an elapsed time, so it cannot create
/// or improve a leaderboard result.
pub async fn recover_run(
    State(state): State<crate::AppState>,
    Path(run_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<RecoverRunRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return SynesthesiaError::Invalid.response(request_id_value),
    };
    let complete_room_set = payload.completed_room_ids.len() == ROOM_IDS.len()
        && ROOM_IDS.iter().all(|room_id| {
            payload
                .completed_room_ids
                .iter()
                .any(|value| value == room_id)
        });
    if !complete_room_set {
        return SynesthesiaError::Invalid.response(request_id_value);
    }
    let token_hash = match bearer_hash(&headers) {
        Some(hash) => hash,
        None => return SynesthesiaError::Unauthorized.response(request_id_value),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let updated = sqlx::query(
        r#"
        UPDATE synesthesia_runs
        SET recovery_completed_at = COALESCE(recovery_completed_at, now()), updated_at = now()
        WHERE workspace_id = $1 AND id = $2 AND run_token_hash = $3
          AND campaign_slug = $4
        "#,
    )
    .bind(workspace_id)
    .bind(run_id)
    .bind(&token_hash)
    .bind(CAMPAIGN_SLUG)
    .execute(state.ticketing.pool())
    .await;
    match updated {
        Ok(result) if result.rows_affected() == 1 => {}
        Ok(_) => return SynesthesiaError::Unauthorized.response(request_id_value),
        Err(error) => return SynesthesiaError::sqlx(error).response(request_id_value),
    }
    match completion_response(&state, workspace_id, run_id, true).await {
        Ok(response) => (
            StatusCode::OK,
            [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
            Json(response),
        )
            .into_response(),
        Err(error) => error.response(request_id_value),
    }
}

async fn completion_response(
    state: &crate::AppState,
    workspace_id: Uuid,
    run_id: Uuid,
    issue_handoff: bool,
) -> Result<CompleteRunResponse, SynesthesiaError> {
    let linked_to_fan = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT fan_id IS NOT NULL
        FROM synesthesia_runs
        WHERE workspace_id = $1 AND id = $2
          AND (completed_at IS NOT NULL OR recovery_completed_at IS NOT NULL)
        "#,
    )
    .bind(workspace_id)
    .bind(run_id)
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(SynesthesiaError::sqlx)?
    .ok_or(SynesthesiaError::Conflict)?;

    let (handoff_code, handoff_expires_at) = if linked_to_fan || !issue_handoff {
        (None, None)
    } else {
        let code = random_token().map_err(|()| SynesthesiaError::Unavailable)?;
        let hash = Sha256::digest(code.as_bytes()).to_vec();
        let expires_at =
            time::OffsetDateTime::now_utc() + time::Duration::minutes(HANDOFF_TTL_MINUTES);
        sqlx::query(
            r#"
            UPDATE synesthesia_runs
            SET handoff_token_hash = $3, handoff_expires_at = $4, updated_at = now()
            WHERE workspace_id = $1 AND id = $2 AND fan_id IS NULL
              AND (completed_at IS NOT NULL OR recovery_completed_at IS NOT NULL)
            "#,
        )
        .bind(workspace_id)
        .bind(run_id)
        .bind(hash)
        .bind(expires_at)
        .execute(state.ticketing.pool())
        .await
        .map_err(SynesthesiaError::sqlx)?;
        (Some(code), Some(expires_at))
    };

    let next_event = sqlx::query_as::<_, SynesthesiaNextEvent>(
        r#"
        SELECT event.slug, event.title, event.venue, city.name AS city, event.starts_at
        FROM events AS event
        LEFT JOIN cities AS city ON city.id = event.city_id
        WHERE event.workspace_id = $1
          AND event.status = 'published'
          AND event.starts_at > now()
        ORDER BY event.starts_at, event.id
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(SynesthesiaError::sqlx)?;

    Ok(CompleteRunResponse {
        completed: true,
        linked_to_fan,
        handoff_code,
        handoff_expires_at,
        next_event,
    })
}

/// Read mutable completion/link state without rotating the short-lived handoff.
///
/// This endpoint exists specifically so Synesthesia can discover that My Signal
/// consumed a handoff after the player returns to the game. A read must never
/// invalidate the code another surface is currently using.
pub async fn completion_context(
    State(state): State<crate::AppState>,
    Path(run_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let token_hash = match bearer_hash(&headers) {
        Some(hash) => hash,
        None => return SynesthesiaError::Unauthorized.response(request_id_value),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    match authorize_completed_run(&state, workspace_id, run_id, &token_hash).await {
        Ok(true) => {}
        Ok(false) => return SynesthesiaError::Unauthorized.response(request_id_value),
        Err(error) => return error.response(request_id_value),
    }
    match completion_response(&state, workspace_id, run_id, false).await {
        Ok(response) => (
            StatusCode::OK,
            [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
            Json(response),
        )
            .into_response(),
        Err(error) => error.response(request_id_value),
    }
}

/// Explicitly issue a fresh short-lived handoff for My Signal. Token rotation is
/// a user action, never a side effect of refreshing status.
pub async fn issue_handoff(
    State(state): State<crate::AppState>,
    Path(run_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let token_hash = match bearer_hash(&headers) {
        Some(hash) => hash,
        None => return SynesthesiaError::Unauthorized.response(request_id_value),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    match authorize_completed_run(&state, workspace_id, run_id, &token_hash).await {
        Ok(true) => {}
        Ok(false) => return SynesthesiaError::Unauthorized.response(request_id_value),
        Err(error) => return error.response(request_id_value),
    }
    match completion_response(&state, workspace_id, run_id, true).await {
        Ok(response) => (
            StatusCode::OK,
            [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
            Json(response),
        )
            .into_response(),
        Err(error) => error.response(request_id_value),
    }
}

