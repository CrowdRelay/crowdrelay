pub async fn summary(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    match run_with_timeout(state.ops.operation_timeout, load_summary(&state.ops)).await {
        Ok(summary) => private_json(StatusCode::OK, summary),
        Err(error) => error.into_response(request_id(&headers)),
    }
}

/// Returns an aggregate-only owner dashboard for Virya Signal.
///
/// The core summary is required. Top-city aggregation is deliberately treated
/// as an optional source so a secondary analytics query cannot take down the
/// whole control plane.
pub async fn signal_overview(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let summary_future =
        run_with_timeout(state.ops.operation_timeout, load_signal_summary(&state.ops));
    let cities_future = run_with_timeout(
        state.ops.operation_timeout,
        load_signal_top_cities(&state.ops),
    );
    let (summary_result, cities_result) = tokio::join!(summary_future, cities_future);

    let summary = match summary_result {
        Ok(summary) => summary,
        Err(error) => return error.into_response(request_id(&headers)),
    };

    let mut unavailable_sources = Vec::new();
    let top_cities = match cities_result {
        Ok(cities) => cities,
        Err(error) => {
            tracing::warn!(
                error_kind = ?error,
                "signal top-city aggregation is unavailable"
            );
            unavailable_sources.push("top_cities");
            Vec::new()
        }
    };

    private_json(
        StatusCode::OK,
        signal_overview_from_row(summary, top_cities, unavailable_sources),
    )
}

pub async fn list_outbox(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    let limit = match page_size(query.limit) {
        Ok(limit) => limit,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let result = match query.status {
        Some(status) => {
            run_with_timeout(
                state.ops.operation_timeout,
                sqlx::query_as::<_, OutboxItem>(
                    r#"
                    SELECT id, event_type, event_version, status, attempts, max_attempts,
                           available_at, last_error_kind, created_at, updated_at,
                           delivered_at, dead_at
                    FROM outbox_events
                    WHERE workspace_id = $1 AND status = $2
                    ORDER BY created_at DESC, id DESC
                    LIMIT $3
                    "#,
                )
                .bind(state.ops.workspace_id.into_uuid())
                .bind(status.as_str())
                .bind(limit)
                .fetch_all(&state.ops.pool),
            )
            .await
        }
        None => {
            run_with_timeout(
                state.ops.operation_timeout,
                sqlx::query_as::<_, OutboxItem>(
                    r#"
                    SELECT id, event_type, event_version, status, attempts, max_attempts,
                           available_at, last_error_kind, created_at, updated_at,
                           delivered_at, dead_at
                    FROM outbox_events
                    WHERE workspace_id = $1
                    ORDER BY created_at DESC, id DESC
                    LIMIT $2
                    "#,
                )
                .bind(state.ops.workspace_id.into_uuid())
                .bind(limit)
                .fetch_all(&state.ops.pool),
            )
            .await
        }
    };
    match result {
        Ok(items) => private_json(StatusCode::OK, items),
        Err(error) => error.into_response(request_id(&headers)),
    }
}

pub async fn list_deliveries(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    let limit = match page_size(query.limit) {
        Ok(limit) => limit,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let result = match query.status {
        Some(status) => {
            run_with_timeout(
                state.ops.operation_timeout,
                sqlx::query_as::<_, DeliveryItem>(
                    r#"
                    SELECT delivery.id, delivery.outbox_event_id, event.event_type,
                           endpoint.name AS endpoint_name, endpoint.active AS endpoint_active,
                           delivery.status, delivery.attempt_count, delivery.max_attempts,
                           delivery.available_at, delivery.last_response_status,
                           delivery.last_error_kind, delivery.created_at, delivery.updated_at,
                           delivery.delivered_at, delivery.dead_at
                    FROM webhook_deliveries AS delivery
                    JOIN outbox_events AS event
                      ON event.workspace_id = delivery.workspace_id
                     AND event.id = delivery.outbox_event_id
                    JOIN webhook_endpoints AS endpoint
                      ON endpoint.workspace_id = delivery.workspace_id
                     AND endpoint.id = delivery.endpoint_id
                    WHERE delivery.workspace_id = $1 AND delivery.status = $2
                    ORDER BY delivery.created_at DESC, delivery.id DESC
                    LIMIT $3
                    "#,
                )
                .bind(state.ops.workspace_id.into_uuid())
                .bind(status.as_str())
                .bind(limit)
                .fetch_all(&state.ops.pool),
            )
            .await
        }
        None => {
            run_with_timeout(
                state.ops.operation_timeout,
                sqlx::query_as::<_, DeliveryItem>(
                    r#"
                    SELECT delivery.id, delivery.outbox_event_id, event.event_type,
                           endpoint.name AS endpoint_name, endpoint.active AS endpoint_active,
                           delivery.status, delivery.attempt_count, delivery.max_attempts,
                           delivery.available_at, delivery.last_response_status,
                           delivery.last_error_kind, delivery.created_at, delivery.updated_at,
                           delivery.delivered_at, delivery.dead_at
                    FROM webhook_deliveries AS delivery
                    JOIN outbox_events AS event
                      ON event.workspace_id = delivery.workspace_id
                     AND event.id = delivery.outbox_event_id
                    JOIN webhook_endpoints AS endpoint
                      ON endpoint.workspace_id = delivery.workspace_id
                     AND endpoint.id = delivery.endpoint_id
                    WHERE delivery.workspace_id = $1
                    ORDER BY delivery.created_at DESC, delivery.id DESC
                    LIMIT $2
                    "#,
                )
                .bind(state.ops.workspace_id.into_uuid())
                .bind(limit)
                .fetch_all(&state.ops.pool),
            )
            .await
        }
    };
    match result {
        Ok(items) => private_json(StatusCode::OK, items),
        Err(error) => error.into_response(request_id(&headers)),
    }
}

pub async fn delivery_details(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let id = match parse_id(&id) {
        Ok(id) => id,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    match run_with_timeout(state.ops.operation_timeout, load_delivery(&state.ops, id)).await {
        Ok(Some(details)) => private_json(StatusCode::OK, details),
        Ok(None) => Problem::not_found(request_id(&headers))
            .private()
            .into_response(),
        Err(error) => error.into_response(request_id(&headers)),
    }
}

pub async fn clear_dead_deliveries(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let idempotency_key = match idempotency_key(&headers) {
        Ok(key) => key,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let request_id_value = request_id(&headers);
    let future = clear_dead_deliveries_transaction(
        &state.ops,
        &idempotency_key,
        request_id_value.as_deref(),
    );
    match run_with_timeout(state.ops.operation_timeout, future).await {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => error.into_response(request_id_value),
    }
}

async fn clear_dead_deliveries_transaction(
    state: &OpsState,
    idempotency_key: &str,
    request_id: Option<&str>,
) -> Result<ClearDeadDeliveriesResult, OpsError> {
    let mut transaction = state.pool.begin().await.map_err(OpsError::sqlx)?;
    let operation_id = Uuid::now_v7();
    let workspace_id = state.workspace_id.into_uuid();
    let action = "clear_dead_deliveries";
    let target_type = "delivery_queue";
    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO operator_actions (
            id, workspace_id, action, target_type, target_id,
            idempotency_key, request_id, details
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        ON CONFLICT (workspace_id, idempotency_key) DO NOTHING
        RETURNING id
        "#,
    )
    .bind(operation_id)
    .bind(workspace_id)
    .bind(action)
    .bind(target_type)
    .bind(workspace_id)
    .bind(idempotency_key)
    .bind(request_id)
    .bind(json!({
        "from_status": "dead",
        "to_status": "cancelled",
        "scope": "webhook_deliveries",
    }))
    .fetch_optional(&mut *transaction)
    .await
    .map_err(OpsError::sqlx)?;

    if inserted.is_none() {
        let existing = load_existing_action(&mut transaction, state, idempotency_key).await?;
        transaction.commit().await.map_err(OpsError::sqlx)?;
        if existing.action != action
            || existing.target_type != target_type
            || existing.target_id != workspace_id
        {
            return Err(OpsError::Conflict);
        }
        return Ok(ClearDeadDeliveriesResult {
            operation_id: existing.id,
            cleared: 0,
            status: "cancelled",
            replayed: true,
        });
    }

    // Clearing the operator dead queue is an acknowledgement, not destructive
    // deletion. Preserve attempt/error history and the parent outbox row; moving
    // only `dead` deliveries to the existing terminal `cancelled` state keeps
    // materialization/idempotency invariants intact and leaves pending/sent rows
    // completely untouched. Normal retention later removes parent+children.
    let cleared = sqlx::query(
        r#"
        UPDATE webhook_deliveries
        SET status = 'cancelled',
            locked_at = NULL, lock_owner = NULL, lease_expires_at = NULL,
            dead_at = NULL, cancelled_at = now(), updated_at = now()
        WHERE workspace_id = $1 AND status = 'dead'
        "#,
    )
    .bind(workspace_id)
    .execute(&mut *transaction)
    .await
    .map_err(OpsError::sqlx)?
    .rows_affected();

    transaction.commit().await.map_err(OpsError::sqlx)?;
    Ok(ClearDeadDeliveriesResult {
        operation_id,
        cleared,
        status: "cancelled",
        replayed: false,
    })
}

pub async fn retry_outbox(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !matches!(
        crate::ecosystem::feature_enabled(&state, "automatic_retry_enabled").await,
        Ok(true)
    ) {
        return Problem::service_unavailable(request_id(&headers))
            .private()
            .into_response();
    }
    retry(&state.ops, &headers, "outbox", &id).await
}

pub async fn retry_delivery(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !matches!(
        crate::ecosystem::feature_enabled(&state, "automatic_retry_enabled").await,
        Ok(true)
    ) {
        return Problem::service_unavailable(request_id(&headers))
            .private()
            .into_response();
    }
    retry(&state.ops, &headers, "delivery", &id).await
}

async fn retry(state: &OpsState, headers: &HeaderMap, target: &'static str, id: &str) -> Response {
    let id = match parse_id(id) {
        Ok(id) => id,
        Err(error) => return error.into_response(request_id(headers)),
    };
    let idempotency_key = match idempotency_key(headers) {
        Ok(key) => key,
        Err(error) => return error.into_response(request_id(headers)),
    };
    let request_id_value = request_id(headers);
    let future = retry_transaction(
        state,
        target,
        id,
        &idempotency_key,
        request_id_value.as_deref(),
    );
    match run_with_timeout(state.operation_timeout, future).await {
        Ok(result) => private_json(StatusCode::OK, result),
        Err(error) => error.into_response(request_id_value),
    }
}

async fn retry_transaction(
    state: &OpsState,
    target: &'static str,
    target_id: Uuid,
    idempotency_key: &str,
    request_id: Option<&str>,
) -> Result<RetryResult, OpsError> {
    let mut transaction = state.pool.begin().await.map_err(OpsError::sqlx)?;
    let action = format!("retry_{target}");
    let operation_id = Uuid::now_v7();
    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO operator_actions (
            id, workspace_id, action, target_type, target_id,
            idempotency_key, request_id, details
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        ON CONFLICT (workspace_id, idempotency_key) DO NOTHING
        RETURNING id
        "#,
    )
    .bind(operation_id)
    .bind(state.workspace_id.into_uuid())
    .bind(&action)
    .bind(target)
    .bind(target_id)
    .bind(idempotency_key)
    .bind(request_id)
    .bind(json!({"requested_status": "pending"}))
    .fetch_optional(&mut *transaction)
    .await
    .map_err(OpsError::sqlx)?;

    if inserted.is_none() {
        let existing = load_existing_action(&mut transaction, state, idempotency_key).await?;
        transaction.commit().await.map_err(OpsError::sqlx)?;
        if existing.action != action
            || existing.target_type != target
            || existing.target_id != target_id
        {
            return Err(OpsError::Conflict);
        }
        return Ok(RetryResult {
            operation_id: existing.id,
            target_type: target,
            target_id,
            status: "pending",
            replayed: true,
        });
    }

    let rows = match target {
        "outbox" => retry_dead_outbox(&mut transaction, state, target_id).await?,
        "delivery" => retry_dead_delivery(&mut transaction, state, target_id).await?,
        _ => return Err(OpsError::BadRequest),
    };
    if rows == 0 {
        return Err(classify_retry_miss(&mut transaction, state, target, target_id).await?);
    }
    transaction.commit().await.map_err(OpsError::sqlx)?;
    Ok(RetryResult {
        operation_id,
        target_type: target,
        target_id,
        status: "pending",
        replayed: false,
    })
}

async fn retry_dead_outbox(
    transaction: &mut Transaction<'_, Postgres>,
    state: &OpsState,
    id: Uuid,
) -> Result<u64, OpsError> {
    let result = sqlx::query(
        r#"
        UPDATE outbox_events
        SET status = 'pending', available_at = now(), max_attempts = attempts + 1,
            locked_at = NULL, lock_owner = NULL, lease_expires_at = NULL,
            last_error_kind = NULL, delivered_at = NULL, dead_at = NULL
        WHERE workspace_id = $1 AND id = $2 AND status = 'dead'
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(id)
    .execute(&mut **transaction)
    .await
    .map_err(OpsError::sqlx)?;
    Ok(result.rows_affected())
}

async fn retry_dead_delivery(
    transaction: &mut Transaction<'_, Postgres>,
    state: &OpsState,
    id: Uuid,
) -> Result<u64, OpsError> {
    let result = sqlx::query(
        r#"
        UPDATE webhook_deliveries AS delivery
        SET status = 'pending', available_at = now(), max_attempts = attempt_count + 1,
            locked_at = NULL, lock_owner = NULL, lease_expires_at = NULL,
            last_response_status = NULL, last_error_kind = NULL,
            delivered_at = NULL, dead_at = NULL, cancelled_at = NULL
        FROM webhook_endpoints AS endpoint
        WHERE delivery.workspace_id = $1 AND delivery.id = $2
          AND delivery.status = 'dead'
          AND endpoint.workspace_id = delivery.workspace_id
          AND endpoint.id = delivery.endpoint_id
          AND endpoint.active
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(id)
    .execute(&mut **transaction)
    .await
    .map_err(OpsError::sqlx)?;
    Ok(result.rows_affected())
}

async fn classify_retry_miss(
    transaction: &mut Transaction<'_, Postgres>,
    state: &OpsState,
    target: &str,
    id: Uuid,
) -> Result<OpsError, OpsError> {
    let status = if target == "delivery" {
        sqlx::query_as::<_, (String, bool)>(
            r#"
            SELECT delivery.status, endpoint.active
            FROM webhook_deliveries AS delivery
            JOIN webhook_endpoints AS endpoint
              ON endpoint.workspace_id = delivery.workspace_id
             AND endpoint.id = delivery.endpoint_id
            WHERE delivery.workspace_id = $1 AND delivery.id = $2
            "#,
        )
        .bind(state.workspace_id.into_uuid())
        .bind(id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(OpsError::sqlx)?
    } else {
        sqlx::query_scalar::<_, String>(
            "SELECT status FROM outbox_events WHERE workspace_id = $1 AND id = $2",
        )
        .bind(state.workspace_id.into_uuid())
        .bind(id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(OpsError::sqlx)?
        .map(|status| (status, true))
    };
    Ok(match status {
        None => OpsError::NotFound,
        Some((_, false)) => OpsError::InactiveEndpoint,
        Some(_) => OpsError::Conflict,
    })
}

async fn load_existing_action(
    transaction: &mut Transaction<'_, Postgres>,
    state: &OpsState,
    idempotency_key: &str,
) -> Result<ExistingAction, OpsError> {
    sqlx::query_as::<_, ExistingAction>(
        r#"
        SELECT id, action, target_type, target_id
        FROM operator_actions
        WHERE workspace_id = $1 AND idempotency_key = $2
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(idempotency_key)
    .fetch_one(&mut **transaction)
    .await
    .map_err(OpsError::sqlx)
}

fn signal_overview_from_row(
    row: SignalSummaryRow,
    top_cities: Vec<SignalCitySummary>,
    unavailable_sources: Vec<&'static str>,
) -> SignalOverview {
    SignalOverview {
        generated_at: OffsetDateTime::now_utc(),
        summary: SignalFanSummary {
            total_fans: row.total_fans,
            active_fans: row.active_fans,
            pending_fans: row.pending_fans,
            unsubscribed_fans: row.unsubscribed_fans,
            suppressed_fans: row.suppressed_fans,
            marketing_opted_in: row.marketing_opted_in,
            nearby_enabled: row.nearby_enabled,
        },
        activity: SignalActivitySummary {
            new_fans_7d: row.new_fans_7d,
            new_fans_30d: row.new_fans_30d,
            referral_attributions_total: row.referral_attributions_total,
            referral_attributions_30d: row.referral_attributions_30d,
            event_interests_total: row.event_interests_total,
            event_interests_30d: row.event_interests_30d,
            nearby_notifications_30d: row.nearby_notifications_30d,
            pending_city_requests: row.pending_city_requests,
        },
        top_cities,
        unavailable_sources,
    }
}
