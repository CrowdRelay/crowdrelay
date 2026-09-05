// Action Ledger API endpoints — kept out of ops_timeline.rs to preserve
// the source-size ratchet. This file is included into the `ops` module,
// so it deliberately shares its private database state and helpers.

#[derive(Debug, Serialize, FromRow)]
pub struct ActionLedgerEntry {
    pub action_id: String,
    pub state: String,
    pub trace_id: Option<String>,
    pub causation_id: Option<String>,
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
                causation_id::text AS causation_id,
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
                causation_id::text AS causation_id,
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
            causation_id::text AS causation_id,
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

// ── Brain cycle runs ──

/// One brain cycle: when it ran, how it was triggered, whether every phase
/// completed, and what it produced.
#[derive(Debug, Serialize, FromRow)]
pub struct CycleRunEntry {
    pub cycle_id: String,
    pub trigger: String,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    pub finished_at: Option<String>,
    pub duration_ms: Option<i32>,
    /// `succeeded`, `degraded`, or absent when the cycle never finished --
    /// which means the process died mid-cycle, and is otherwise
    /// indistinguishable from a cycle that ran and decided nothing.
    pub outcome: Option<String>,
    pub decisions_recorded: i32,
    pub actions_created: i32,
}

/// GET /v1/admin/ops/cycles — the last brain cycles, newest first.
pub async fn list_cycles(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Query(query): Query<ActionLedgerQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(20).clamp(1, 200);
    match run_with_timeout(
        state.ops.operation_timeout,
        load_cycle_runs(&state.ops, query.state.as_deref(), limit),
    )
    .await
    {
        Ok(entries) => private_json(StatusCode::OK, entries),
        Err(error) => error.into_response(request_id(&headers)),
    }
}

/// `state=degraded` narrows to the cycles worth looking at, which is the only
/// filter anyone applies twice.
async fn load_cycle_runs(
    ops: &OpsState,
    outcome_filter: Option<&str>,
    limit: i64,
) -> Result<Vec<CycleRunEntry>, OpsError> {
    sqlx::query_as::<_, CycleRunEntry>(
        r#"
        SELECT id::text AS cycle_id,
               trigger,
               started_at,
               to_char(finished_at, 'YYYY-MM-DD"T"HH24:MI:SSOF:00') AS finished_at,
               duration_ms,
               outcome,
               decisions_recorded,
               actions_created
        FROM viryaos_autopilot_cycle_runs
        WHERE workspace_id = $1
          AND ($2::text IS NULL OR outcome IS NOT DISTINCT FROM $2)
        ORDER BY started_at DESC
        LIMIT $3
        "#,
    )
    .bind(ops.workspace_id.into_uuid())
    .bind(outcome_filter)
    .bind(limit)
    .fetch_all(&ops.pool)
    .await
    .map_err(OpsError::sqlx)
}
