// Action Ledger API endpoints — kept out of ops_timeline.rs to preserve
// the source-size ratchet. This file is included into the `ops` module,
// so it deliberately shares its private database state and helpers.

#[derive(Debug, Serialize, FromRow)]
pub struct ActionLedgerEntry {
    pub action_id: String,
    pub state: String,
    pub trace_id: Option<String>,
    pub decision_id: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub state_entered_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    pub transition_count: i32,
    pub previous_state: Option<String>,
    pub reconciliation_count: i32,
    pub last_reconciliation_error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ActionLedgerQuery {
    pub state: Option<String>,
    pub limit: Option<i64>,
}

/// GET /v1/admin/ops/actions — list action ledger entries.
pub async fn list_actions(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Query(query): Query<ActionLedgerQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(50).clamp(1, 250);
    match run_with_timeout(
        state.ops.operation_timeout,
        load_action_ledger(&state.ops, query.state.as_deref(), limit),
    )
    .await
    {
        Ok(entries) => private_json(StatusCode::OK, entries),
        Err(error) => error.into_response(request_id(&headers)),
    }
}

async fn load_action_ledger(
    ops: &OpsState,
    state_filter: Option<&str>,
    limit: i64,
) -> Result<Vec<ActionLedgerEntry>, OpsError> {
    let entries = if let Some(state) = state_filter {
        sqlx::query_as::<_, ActionLedgerEntry>(
            r#"
            SELECT
                action_id::text AS action_id,
                state,
                trace_id::text AS trace_id,
                decision_id::text AS decision_id,
                state_entered_at,
                updated_at,
                transition_count,
                previous_state,
                reconciliation_count,
                last_reconciliation_error
            FROM viryaos_action_ledger
            WHERE workspace_id = $1 AND state = $2
            ORDER BY state_entered_at DESC
            LIMIT $3
            "#,
        )
        .bind(ops.workspace_id.into_uuid())
        .bind(state)
        .bind(limit)
        .fetch_all(&ops.pool)
        .await
    } else {
        sqlx::query_as::<_, ActionLedgerEntry>(
            r#"
            SELECT
                action_id::text AS action_id,
                state,
                trace_id::text AS trace_id,
                decision_id::text AS decision_id,
                state_entered_at,
                updated_at,
                transition_count,
                previous_state,
                reconciliation_count,
                last_reconciliation_error
            FROM viryaos_action_ledger
            WHERE workspace_id = $1
            ORDER BY state_entered_at DESC
            LIMIT $2
            "#,
        )
        .bind(ops.workspace_id.into_uuid())
        .bind(limit)
        .fetch_all(&ops.pool)
        .await
    };
    entries.map_err(OpsError::sqlx)
}

/// GET /v1/admin/ops/actions/{action_id} — get a single action ledger entry.
pub async fn get_action(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Path(raw_action_id): Path<String>,
) -> Response {
    let action_id = match parse_uuid(&raw_action_id) {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    match run_with_timeout(
        state.ops.operation_timeout,
        load_single_action(&state.ops, action_id),
    )
    .await
    {
        Ok(entry) => private_json(StatusCode::OK, entry),
        Err(error) => error.into_response(request_id(&headers)),
    }
}

async fn load_single_action(
    ops: &OpsState,
    action_id: Uuid,
) -> Result<ActionLedgerEntry, OpsError> {
    sqlx::query_as::<_, ActionLedgerEntry>(
        r#"
        SELECT
            action_id::text AS action_id,
            state,
            trace_id::text AS trace_id,
            decision_id::text AS decision_id,
            state_entered_at,
            updated_at,
            transition_count,
            previous_state,
            reconciliation_count,
            last_reconciliation_error
        FROM viryaos_action_ledger
        WHERE workspace_id = $1 AND action_id = $2
        "#,
    )
    .bind(ops.workspace_id.into_uuid())
    .bind(action_id)
    .fetch_optional(&ops.pool)
    .await
    .map_err(OpsError::sqlx)?
    .ok_or(OpsError::NotFound)
}

fn parse_uuid(value: &str) -> Result<Uuid, OpsError> {
    Uuid::parse_str(value.trim()).map_err(|_| OpsError::BadRequest)
}
