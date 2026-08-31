#[derive(Debug, Serialize)]
struct AttentionEcosystemOverview {
    schema_version: u32,
    flags: Vec<crate::ecosystem::FeatureFlag>,
    last_reconciliation: Option<crate::ecosystem::ReconciliationRun>,
    open_findings: i64,
    next_event: Option<crate::ecosystem::OverviewEvent>,
    bandsintown_sync: Option<crate::ecosystem::BandsintownSyncStatus>,
}

/// Lightweight summary of a pending autopilot action — just the fields the
/// AttentionInbox needs to render an approval item. NOT the full
/// `PendingAutopilotAction` (which includes payload, briefing, assignee,
/// executor readiness, etc.) — the attention snapshot is a summary view,
/// not a detail modal.
#[derive(Debug, Serialize)]
struct PendingActionSummary {
    id: uuid::Uuid,
    context: String,
    action_kind: String,
    subject_kind: String,
    #[serde(with = "time::serde::rfc3339::option")]
    approval_expires_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct PendingActionSummaryRow {
    id: uuid::Uuid,
    context: String,
    action_kind: String,
    subject_kind: String,
    approval_expires_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize)]
struct OperatorAttentionSnapshot {
    summary: OpsSummary,
    alerts: Vec<OpsAlert>,
    dead_outbox: Vec<OutboxItem>,
    dead_deliveries: Vec<DeliveryItem>,
    dead_push: Vec<PushDeliveryItem>,
    ecosystem: AttentionEcosystemOverview,
    findings: Vec<crate::ecosystem::ReconciliationFinding>,
    /// Pending autopilot actions awaiting human approval. Comes from the
    /// same authoritative query as `load_control_overview` — just the
    /// summary fields the inbox renders, not the full action detail.
    needs_you: Vec<PendingActionSummary>,
    /// Count of opportunities awaiting approval. Derived from authoritative
    /// action state, not from rendered UI items.
    awaiting_approval: i64,
}

pub async fn attention(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let timeout_duration = state.ops.operation_timeout;
    let summary = run_with_timeout(timeout_duration, load_summary(&state.ops));
    let alerts = run_with_timeout(timeout_duration, load_alerts(&state.ops));
    let dead_outbox = run_with_timeout(timeout_duration, load_dead_outbox(&state.ops));
    let dead_deliveries = run_with_timeout(timeout_duration, load_dead_deliveries(&state.ops));
    let dead_push = run_with_timeout(timeout_duration, load_dead_push(&state.ops));
    let ecosystem = run_with_timeout(timeout_duration, load_attention_ecosystem(&state));
    let findings = run_with_timeout(timeout_duration, load_open_findings(&state));
    let needs_you = run_with_timeout(timeout_duration, load_needs_you(&state.ops));
    let awaiting_approval = run_with_timeout(timeout_duration, load_awaiting_approval(&state.ops));

    let (
        summary, alerts, dead_outbox, dead_deliveries, dead_push,
        ecosystem, findings, needs_you, awaiting_approval,
    ) = tokio::join!(
        summary, alerts, dead_outbox, dead_deliveries, dead_push,
        ecosystem, findings, needs_you, awaiting_approval,
    );

    let request_id_value = request_id(&headers);
    let summary = match summary {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id_value),
    };
    let alerts = match alerts {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let dead_outbox = match dead_outbox {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let dead_deliveries = match dead_deliveries {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let dead_push = match dead_push {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let ecosystem = match ecosystem {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let findings = match findings {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let needs_you = match needs_you {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };
    let awaiting_approval = match awaiting_approval {
        Ok(value) => value,
        Err(error) => return error.into_response(request_id(&headers)),
    };

    private_json(
        StatusCode::OK,
        OperatorAttentionSnapshot {
            summary,
            alerts,
            dead_outbox,
            dead_deliveries,
            dead_push,
            ecosystem,
            findings,
            needs_you,
            awaiting_approval,
        },
    )
}

/// Open watchdog alerts, plus the ones that recovered in the last 24 hours.
///
/// Recovered rows stay visible for a day because the watchdog only re-evaluates
/// every five minutes: an operator who fixed the cause needs to see that the
/// alert closed by itself rather than wonder whether the count is stuck.
async fn load_alerts(state: &OpsState) -> Result<Vec<OpsAlert>, OpsError> {
    sqlx::query_as::<_, OpsAlert>(
        r#"
        SELECT alert_key, severity, summary, active, first_seen_at, last_seen_at,
               last_alerted_at, recovered_at, details
        FROM viryaos_ops_alert_state
        WHERE workspace_id = $1
          AND (active OR recovered_at >= now() - INTERVAL '24 hours')
        ORDER BY active DESC,
                 (severity = 'critical') DESC,
                 last_seen_at DESC,
                 alert_key
        LIMIT 50
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_all(&state.pool)
    .await
    .map_err(OpsError::sqlx)
}

async fn load_dead_outbox(state: &OpsState) -> Result<Vec<OutboxItem>, OpsError> {
    sqlx::query_as::<_, OutboxItem>(
        r#"
        SELECT id, event_type, event_version, status, attempts, max_attempts,
               available_at, last_error_kind, created_at, updated_at,
               delivered_at, dead_at
        FROM outbox_events
        WHERE workspace_id = $1 AND status = 'dead'
        ORDER BY created_at DESC, id DESC
        LIMIT 50
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_all(&state.pool)
    .await
    .map_err(OpsError::sqlx)
}

async fn load_dead_deliveries(state: &OpsState) -> Result<Vec<DeliveryItem>, OpsError> {
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
        WHERE delivery.workspace_id = $1 AND delivery.status = 'dead'
        ORDER BY delivery.created_at DESC, delivery.id DESC
        LIMIT 50
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_all(&state.pool)
    .await
    .map_err(OpsError::sqlx)
}

async fn load_dead_push(state: &OpsState) -> Result<Vec<PushDeliveryItem>, OpsError> {
    sqlx::query_as::<_, PushDeliveryItem>(
        r#"
        SELECT id, fan_id, source_kind, title, status, attempt_count,
               error_code, available_at, created_at, delivered_at, completed_at
        FROM fan_push_deliveries
        WHERE workspace_id = $1 AND status IN ('failed', 'ambiguous')
          AND error_code IS DISTINCT FROM 'preference_disabled'
        ORDER BY created_at DESC, id DESC
        LIMIT 50
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_all(&state.pool)
    .await
    .map_err(OpsError::sqlx)
}

async fn load_attention_ecosystem(
    state: &crate::AppState,
) -> Result<AttentionEcosystemOverview, OpsError> {
    // Seed the lazy defaults exactly as `/ecosystem/overview` does. Without
    // this a workspace whose flags have never been written reports an empty
    // flag list here while the dedicated endpoint reports the full default
    // set, so the two views of the same tenant disagree.
    crate::ecosystem::ensure_default_flags(state)
        .await
        .map_err(|_| OpsError::Unexpected)?;
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let flags = sqlx::query_as::<_, crate::ecosystem::FeatureFlag>(
        r#"
        SELECT key, enabled, reason, version, updated_at
        FROM ecosystem_feature_flags
        WHERE workspace_id = $1
        ORDER BY key
        "#,
    )
    .bind(workspace_id)
    .fetch_all(state.ticketing.pool());
    let last_reconciliation = sqlx::query_as::<_, crate::ecosystem::ReconciliationRun>(
        r#"
        SELECT id, status, trigger, finding_count, started_at, finished_at
        FROM reconciliation_runs
        WHERE workspace_id = $1
        ORDER BY started_at DESC, id DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .fetch_optional(state.ticketing.pool());
    let open_findings = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM reconciliation_findings WHERE workspace_id = $1 AND resolved_at IS NULL",
    )
    .bind(workspace_id)
    .fetch_one(state.ticketing.pool());
    let next_event = sqlx::query_as::<_, crate::ecosystem::OverviewEvent>(
        r#"
        SELECT id, slug, title, venue, starts_at
        FROM events
        WHERE workspace_id = $1 AND status = 'published'
          AND starts_at >= now() - interval '6 hours'
        ORDER BY starts_at, id
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .fetch_optional(state.ticketing.pool());
    let bandsintown_sync = sqlx::query_as::<_, crate::ecosystem::BandsintownSyncStatus>(
        r#"
        SELECT last_synced_at, last_success_at, next_sync_at, consecutive_failures, last_error,
               (sync_lease_until IS NOT NULL AND sync_lease_until > now()) AS in_progress
        FROM event_sources
        WHERE workspace_id = $1 AND provider = 'bandsintown' AND active
        ORDER BY id
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .fetch_optional(state.ticketing.pool());

    let (flags, last_reconciliation, open_findings, next_event, bandsintown_sync) =
        tokio::try_join!(flags, last_reconciliation, open_findings, next_event, bandsintown_sync)
            .map_err(OpsError::sqlx)?;

    Ok(AttentionEcosystemOverview {
        schema_version: crate::ecosystem::SHOW_SNAPSHOT_SCHEMA,
        flags,
        last_reconciliation,
        open_findings,
        next_event,
        bandsintown_sync,
    })
}

async fn load_open_findings(
    state: &crate::AppState,
) -> Result<Vec<crate::ecosystem::ReconciliationFinding>, OpsError> {
    sqlx::query_as::<_, crate::ecosystem::ReconciliationFinding>(
        r#"
        SELECT id, run_id, kind, severity, entity_type, entity_id,
               entity_label, summary, suggested_action, metadata,
               created_at, resolved_at
        FROM reconciliation_findings
        WHERE workspace_id = $1 AND resolved_at IS NULL
        ORDER BY created_at DESC, id DESC
        LIMIT 50
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(OpsError::sqlx)
}

/// Load pending autopilot actions awaiting human approval — the same query
/// as `load_control_overview` branch C, but only the summary fields the
/// AttentionInbox renders. NOT the full `PendingAutopilotAction` with
/// payload, briefing, assignee, and executor readiness.
async fn load_needs_you(state: &OpsState) -> Result<Vec<PendingActionSummary>, OpsError> {
    sqlx::query_as::<_, PendingActionSummaryRow>(
        r#"
        SELECT id, context, action_kind, subject_kind, approval_expires_at
        FROM viryaos_autopilot_actions
        WHERE workspace_id = $1
          AND status = 'awaiting_approval'
          AND (approval_expires_at IS NULL OR approval_expires_at > now())
        ORDER BY created_at, id
        LIMIT 50
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_all(&state.pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(|r| PendingActionSummary {
                id: r.id,
                context: r.context,
                action_kind: r.action_kind,
                subject_kind: r.subject_kind,
                approval_expires_at: r.approval_expires_at,
            })
            .collect()
    })
    .map_err(OpsError::sqlx)
}

/// Count of actions awaiting approval — derived from authoritative action
/// state, not from rendered UI items. Same WHERE clause as `load_needs_you`
/// but returns a count.
async fn load_awaiting_approval(state: &OpsState) -> Result<i64, OpsError> {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)::bigint
        FROM viryaos_autopilot_actions
        WHERE workspace_id = $1
          AND status = 'awaiting_approval'
          AND (approval_expires_at IS NULL OR approval_expires_at > now())
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_one(&state.pool)
    .await
    .map_err(OpsError::sqlx)
}
