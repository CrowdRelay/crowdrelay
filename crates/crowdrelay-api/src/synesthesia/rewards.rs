async fn authorize_completed_run(
    state: &crate::AppState,
    workspace_id: Uuid,
    run_id: Uuid,
    token_hash: &[u8],
) -> Result<bool, SynesthesiaError> {
    sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM synesthesia_runs
            WHERE workspace_id = $1 AND id = $2 AND run_token_hash = $3
              AND (completed_at IS NOT NULL OR recovery_completed_at IS NOT NULL)
        )
        "#,
    )
    .bind(workspace_id)
    .bind(run_id)
    .bind(token_hash)
    .fetch_one(state.ticketing.pool())
    .await
    .map_err(SynesthesiaError::sqlx)
}

pub async fn link_completed_run_to_fan(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<LinkRunRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return SynesthesiaError::Invalid.response(request_id_value),
    };
    let handoff_code = payload.handoff_code.trim().to_ascii_lowercase();
    if handoff_code.len() != 64 || !handoff_code.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return SynesthesiaError::Invalid.response(request_id_value);
    }
    let Some(session) = fan_session_from_headers(&headers) else {
        return SynesthesiaError::Unauthorized.response(request_id_value);
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut transaction = match state.ticketing.pool().begin().await {
        Ok(transaction) => transaction,
        Err(error) => return SynesthesiaError::sqlx(error).response(request_id_value),
    };
    let fan_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        UPDATE fan_sessions AS session
        SET last_seen_at = now()
        FROM fans AS fan
        WHERE session.workspace_id = $1
          AND session.session_token_hash = digest($2, 'sha256')
          AND session.revoked_at IS NULL
          AND session.expires_at > now()
          AND fan.workspace_id = session.workspace_id
          AND fan.id = session.fan_id
          AND fan.status = 'active'
        RETURNING session.fan_id
        "#,
    )
    .bind(workspace_id)
    .bind(session.as_str())
    .fetch_optional(&mut *transaction)
    .await;
    let fan_id = match fan_id {
        Ok(Some(fan_id)) => fan_id,
        Ok(None) => return SynesthesiaError::Unauthorized.response(request_id_value),
        Err(error) => return SynesthesiaError::sqlx(error).response(request_id_value),
    };

    let handoff_hash = Sha256::digest(handoff_code.as_bytes()).to_vec();
    let linked = sqlx::query_as::<_, (Uuid, i16, i64)>(
        r#"
        UPDATE synesthesia_runs
        SET fan_id = $3, linked_at = COALESCE(linked_at, now()), updated_at = now()
        WHERE workspace_id = $1
          AND handoff_token_hash = $2
          AND handoff_expires_at > now()
          AND (completed_at IS NOT NULL OR recovery_completed_at IS NOT NULL)
          AND (fan_id IS NULL OR fan_id = $3)
        RETURNING id, next_room_index, COALESCE(client_total_elapsed_ms, 0)
        "#,
    )
    .bind(workspace_id)
    .bind(handoff_hash)
    .bind(fan_id)
    .fetch_optional(&mut *transaction)
    .await;
    let (run_id, rooms_completed, client_total_elapsed_ms) = match linked {
        Ok(Some(value)) => value,
        Ok(None) => return SynesthesiaError::Conflict.response(request_id_value),
        Err(error) => return SynesthesiaError::sqlx(error).response(request_id_value),
    };
    if let Err(error) = transaction.commit().await {
        return SynesthesiaError::sqlx(error).response(request_id_value);
    }
    (
        StatusCode::OK,
        [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
        Json(LinkRunResponse {
            linked: true,
            run_id,
            rooms_completed,
            client_total_elapsed_ms,
        }),
    )
        .into_response()
}

pub async fn enter_reward_draw(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<RewardEntryRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return SynesthesiaError::Invalid.response(request_id_value),
    };
    let email = match NormalizedEmail::parse(&payload.email) {
        Ok(email) => email,
        Err(_) => return SynesthesiaError::Invalid.response(request_id_value),
    };
    if payload.policy_version.trim().is_empty()
        || payload.policy_version.len() > 120
        || clean_locale(payload.locale.as_deref()).is_none() && payload.locale.is_some()
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

    let result = enter_reward_draw_inner(
        &mut transaction,
        workspace_id,
        &token_hash,
        email.as_str(),
        &payload,
    )
    .await;
    match result {
        Ok(()) => {}
        Err(error) => return error.response(request_id_value),
    }
    if let Err(error) = transaction.commit().await {
        return SynesthesiaError::sqlx(error).response(request_id_value);
    }

    (
        StatusCode::OK,
        [(CACHE_CONTROL, HeaderValue::from_static(PRIVATE_NO_STORE))],
        Json(RewardEntryResponse {
            status: "entered_draw",
            message: "Jesteś w losowaniu jednej z 5 płyt Echoes Of The Modern Mind. Jedno ukończenie = jeden los.",
            draw_size: 5,
        }),
    )
        .into_response()
}

async fn enter_reward_draw_inner(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    token_hash: &[u8],
    normalized_email: &str,
    payload: &RewardEntryRequest,
) -> Result<(), SynesthesiaError> {
    let Some((run_id, campaign_slug)) = sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT id, campaign_slug
        FROM synesthesia_runs
        WHERE workspace_id = $1 AND run_token_hash = $2
          AND (completed_at IS NOT NULL OR recovery_completed_at IS NOT NULL)
        FOR SHARE
        "#,
    )
    .bind(workspace_id)
    .bind(token_hash)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(SynesthesiaError::sqlx)?
    else {
        return Err(SynesthesiaError::Unauthorized);
    };
    if campaign_slug != CAMPAIGN_SLUG {
        return Err(SynesthesiaError::Conflict);
    }

    let draw_is_open = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM reward_draws
            WHERE workspace_id = $1
              AND eligibility_kind = 'synesthesia_completion'
              AND eligibility_ref = $2
              AND status = 'scheduled'
              AND opens_at <= now()
              AND closes_at > now()
        )
        "#,
    )
    .bind(workspace_id)
    .bind(&campaign_slug)
    .fetch_one(&mut **transaction)
    .await
    .map_err(SynesthesiaError::sqlx)?;
    if !draw_is_open {
        return Err(SynesthesiaError::Conflict);
    }

    let fan_id = match sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT id, status
        FROM fans
        WHERE workspace_id = $1 AND normalized_email = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(normalized_email)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(SynesthesiaError::sqlx)?
    {
        Some((_, status)) if status == "suppressed" => return Err(SynesthesiaError::Conflict),
        Some((fan_id, _)) => fan_id,
        None => sqlx::query_scalar::<_, Uuid>(
            r#"
                INSERT INTO fans (workspace_id, normalized_email, locale, status)
                VALUES ($1, $2, $3, 'pending')
                RETURNING id
                "#,
        )
        .bind(workspace_id)
        .bind(normalized_email)
        .bind(clean_locale(payload.locale.as_deref()))
        .fetch_one(&mut **transaction)
        .await
        .map_err(SynesthesiaError::sqlx)?,
    };

    let linked_run = sqlx::query(
        r#"
        UPDATE synesthesia_runs
        SET fan_id = $3, linked_at = COALESCE(linked_at, now()),
            handoff_token_hash = NULL, handoff_expires_at = NULL, updated_at = now()
        WHERE workspace_id = $1 AND id = $2
          AND (fan_id IS NULL OR fan_id = $3)
        "#,
    )
    .bind(workspace_id)
    .bind(run_id)
    .bind(fan_id)
    .execute(&mut **transaction)
    .await
    .map_err(SynesthesiaError::sqlx)?;
    if linked_run.rows_affected() != 1 {
        return Err(SynesthesiaError::Conflict);
    }

    sqlx::query(
        r#"
        INSERT INTO synesthesia_reward_entries (
            workspace_id, campaign_slug, run_id, fan_id, normalized_email,
            policy_version, locale
        ) VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (workspace_id, campaign_slug, normalized_email) DO NOTHING
        "#,
    )
    .bind(workspace_id)
    .bind(&campaign_slug)
    .bind(run_id)
    .bind(fan_id)
    .bind(normalized_email)
    .bind(payload.policy_version.trim())
    .bind(clean_locale(payload.locale.as_deref()))
    .execute(&mut **transaction)
    .await
    .map_err(SynesthesiaError::sqlx)?;

    Ok(())
}

