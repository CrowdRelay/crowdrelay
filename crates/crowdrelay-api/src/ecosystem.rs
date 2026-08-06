//! End-to-end operating controls shared by CrowdRelay, Virya, n8n and mobile.
//!
//! This module is intentionally metadata-only. It never exposes raw QR tokens,
//! buyer e-mail addresses, webhook payloads or provider secrets.

use std::{
    collections::HashMap,
    future::Future,
    sync::OnceLock,
    time::{Duration as StdDuration, Instant},
};

use axum::{
    Json,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, Postgres, Transaction};
use time::{Duration as TimeDuration, OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{sync::RwLock, time::timeout};
use uuid::Uuid;

use crate::{IDEMPOTENCY_KEY, Problem, request_id, ticket_qr::encode_ticket_qr};

const PRIVATE_NO_STORE: &str = "private, no-store";
const SHOW_SNAPSHOT_SCHEMA: u32 = 1;
const MAX_SHOW_PASSES: i64 = 10_000;
const MAX_LIST_LIMIT: i64 = 100;
const FLAG_CACHE_TTL: StdDuration = StdDuration::from_secs(1);
const MAX_FLAG_CACHE_ENTRIES: usize = 256;
const FLAG_KEYS: [(&str, bool); 13] = [
    ("ticket_sales_enabled", true),
    ("ticket_delivery_enabled", true),
    ("gate_redemption_enabled", true),
    ("mailer_enabled", true),
    ("meta_publish_enabled", true),
    ("bandsintown_sync_enabled", true),
    ("n8n_ingress_enabled", true),
    ("automatic_retry_enabled", true),
    ("draw_proofs_enabled", true),
    ("external_proof_anchoring_enabled", false),
    ("merch_inventory_enabled", false),
    ("reward_campaigns_enabled", false),
    ("merch_inventory_writes_enabled", false),
];

#[derive(Clone, Copy)]
struct CachedFlag {
    enabled: bool,
    expires_at: Instant,
}

type FlagCache = HashMap<(Uuid, &'static str), CachedFlag>;

static FLAG_CACHE: OnceLock<RwLock<FlagCache>> = OnceLock::new();

fn flag_cache() -> &'static RwLock<FlagCache> {
    FLAG_CACHE.get_or_init(|| RwLock::new(HashMap::with_capacity(FLAG_KEYS.len())))
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct FeatureFlag {
    key: String,
    enabled: bool,
    reason: Option<String>,
    version: i64,
    updated_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateFlagRequest {
    enabled: bool,
    reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FlagMutationResult {
    flag: FeatureFlag,
    replayed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconcileRequest {
    #[serde(default = "default_reconcile_trigger")]
    trigger: String,
}

fn default_reconcile_trigger() -> String {
    "manual".to_owned()
}

#[derive(Debug, Serialize, FromRow)]
pub struct ReconciliationRun {
    id: Uuid,
    status: String,
    trigger: String,
    finding_count: i32,
    started_at: OffsetDateTime,
    finished_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct ReconciliationFinding {
    id: Uuid,
    run_id: Uuid,
    kind: String,
    severity: String,
    entity_type: String,
    entity_id: Option<Uuid>,
    entity_label: Option<String>,
    summary: String,
    suggested_action: Option<String>,
    metadata: Value,
    created_at: OffsetDateTime,
    resolved_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize)]
pub struct ReconciliationResult {
    run: ReconciliationRun,
    findings: Vec<ReconciliationFinding>,
    replayed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListFindingsQuery {
    limit: Option<i64>,
    open_only: Option<bool>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct ChecklistItem {
    item_key: String,
    status: String,
    note: Option<String>,
    updated_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateChecklistRequest {
    status: String,
    note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ShowChecklist {
    event_id: Uuid,
    event_slug: String,
    event_title: String,
    starts_at: OffsetDateTime,
    items: Vec<ChecklistItem>,
}

#[derive(Debug, Serialize)]
pub struct EmissionResult {
    emitted: i64,
}

#[derive(Debug, Serialize)]
pub struct EcosystemOverview {
    schema_version: u32,
    flags: Vec<FeatureFlag>,
    last_reconciliation: Option<ReconciliationRun>,
    open_findings: i64,
    next_event: Option<OverviewEvent>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct OverviewEvent {
    id: Uuid,
    slug: String,
    title: String,
    venue: Option<String>,
    starts_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
pub struct ShowModeSnapshot {
    schema_version: u32,
    snapshot_id: String,
    event: ShowModeEvent,
    generated_at: String,
    expires_at: String,
    checksum_sha256: String,
    passes: Vec<ShowModePass>,
}

#[derive(Debug, Serialize)]
pub struct ShowModeEvent {
    slug: String,
    title: String,
    venue: Option<String>,
    starts_at: String,
}

#[derive(Debug, Serialize)]
pub struct ShowModePass {
    public_reference: String,
    holder_name: Option<String>,
    holder_email_masked: String,
    ticket_type_name: Option<String>,
    offline_eligible: bool,
    qr_sha256: Option<String>,
}

#[derive(Debug, FromRow)]
struct ShowEventRow {
    id: Uuid,
    slug: String,
    title: String,
    venue: Option<String>,
    starts_at: OffsetDateTime,
    doors_at: Option<OffsetDateTime>,
    ends_at: Option<OffsetDateTime>,
}

#[derive(Debug, FromRow)]
struct ShowPassRow {
    pass_id: Uuid,
    public_reference: String,
    holder_name: Option<String>,
    holder_email: Option<String>,
    issuance_method: String,
    status: String,
    ticket_type_name: Option<String>,
}

#[derive(Debug, FromRow)]
struct ExistingMutation {
    action: String,
    target_type: String,
    target_id: Uuid,
    details: Value,
}

pub async fn overview(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let future = async {
        ensure_default_flags(&state).await?;
        let (flags, last_reconciliation, open_findings, next_event) = tokio::try_join!(
            load_flags(&state),
            load_last_reconciliation(&state),
            count_open_findings(&state),
            load_next_event(&state),
        )?;
        Ok::<_, EcosystemError>(EcosystemOverview {
            schema_version: SHOW_SNAPSHOT_SCHEMA,
            flags,
            last_reconciliation,
            open_findings,
            next_event,
        })
    };
    respond(run(&state, future).await, request_id(&headers))
}

pub async fn list_flags(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let future = async {
        ensure_default_flags(&state).await?;
        load_flags(&state).await
    };
    respond(run(&state, future).await, request_id(&headers))
}

pub async fn update_flag(
    State(state): State<crate::AppState>,
    Path(key): Path<String>,
    headers: HeaderMap,
    payload: Result<Json<UpdateFlagRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return EcosystemError::BadRequest.into_response(request_id_value),
    };
    respond(
        run(&state, update_flag_inner(&state, &headers, &key, payload)).await,
        request_id_value,
    )
}

pub async fn reconcile(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ReconcileRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return EcosystemError::BadRequest.into_response(request_id_value),
    };
    respond(
        run(&state, reconcile_inner(&state, &headers, &payload.trigger)).await,
        request_id_value,
    )
}

pub async fn list_findings(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Query(query): Query<ListFindingsQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(50);
    if !(1..=MAX_LIST_LIMIT).contains(&limit) {
        return EcosystemError::BadRequest.into_response(request_id(&headers));
    }
    let open_only = query.open_only.unwrap_or(true);
    let future = async {
        sqlx::query_as::<_, ReconciliationFinding>(
            r#"
            SELECT id, run_id, kind, severity, entity_type, entity_id,
                   entity_label, summary, suggested_action, metadata,
                   created_at, resolved_at
            FROM reconciliation_findings
            WHERE workspace_id = $1 AND ($2 = false OR resolved_at IS NULL)
            ORDER BY created_at DESC, id DESC
            LIMIT $3
            "#,
        )
        .bind(state.ticketing.workspace_id().into_uuid())
        .bind(open_only)
        .bind(limit)
        .fetch_all(state.ticketing.pool())
        .await
        .map_err(EcosystemError::sqlx)
    };
    respond(run(&state, future).await, request_id(&headers))
}

pub async fn show_checklist(
    State(state): State<crate::AppState>,
    Path(event_slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let future = load_checklist(&state, &event_slug);
    respond(run(&state, future).await, request_id(&headers))
}

pub async fn update_checklist(
    State(state): State<crate::AppState>,
    Path((event_slug, item_key)): Path<(String, String)>,
    headers: HeaderMap,
    payload: Result<Json<UpdateChecklistRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return EcosystemError::BadRequest.into_response(request_id_value),
    };
    respond(
        run(
            &state,
            update_checklist_inner(&state, &headers, &event_slug, &item_key, payload),
        )
        .await,
        request_id_value,
    )
}

pub async fn emit_due_checklists(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let future = emit_due_inner(&state, request_id_value.as_deref());
    let result = run(&state, future).await;
    respond(result, request_id_value)
}

pub async fn show_snapshot(
    State(state): State<crate::AppState>,
    Path(event_slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let future = load_show_snapshot(&state, &event_slug);
    respond(run(&state, future).await, request_id(&headers))
}

/// Reads a persisted feature flag. Hot request paths use a one-second
/// process-local cache; an expired cache entry never masks a database failure,
/// so kill switches still fail closed.
pub(crate) async fn feature_enabled(
    state: &crate::AppState,
    key: &str,
) -> Result<bool, EcosystemError> {
    let (key, default) = flag_definition(key).ok_or(EcosystemError::BadRequest)?;
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    if let Some(enabled) = read_cached_flag(workspace_id, key).await {
        return Ok(enabled);
    }
    let value = sqlx::query_scalar::<_, bool>(
        "SELECT enabled FROM ecosystem_feature_flags WHERE workspace_id = $1 AND key = $2",
    )
    .bind(workspace_id)
    .bind(key)
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(EcosystemError::sqlx)?;
    let enabled = if let Some(enabled) = value {
        enabled
    } else {
        sqlx::query(
            r#"
            INSERT INTO ecosystem_feature_flags (workspace_id, key, enabled, reason)
            VALUES ($1, $2, $3, 'lazy default')
            ON CONFLICT (workspace_id, key) DO NOTHING
            "#,
        )
        .bind(workspace_id)
        .bind(key)
        .bind(default)
        .execute(state.ticketing.pool())
        .await
        .map_err(EcosystemError::sqlx)?;
        // A concurrent operator update may have won the insert race. Read the
        // authoritative row instead of caching the local default.
        sqlx::query_scalar::<_, bool>(
            "SELECT enabled FROM ecosystem_feature_flags WHERE workspace_id = $1 AND key = $2",
        )
        .bind(workspace_id)
        .bind(key)
        .fetch_one(state.ticketing.pool())
        .await
        .map_err(EcosystemError::sqlx)?
    };
    write_cached_flag(workspace_id, key, enabled).await;
    Ok(enabled)
}

async fn ensure_default_flags(state: &crate::AppState) -> Result<(), EcosystemError> {
    sqlx::query(
        r#"
        INSERT INTO ecosystem_feature_flags (workspace_id, key, enabled, reason)
        SELECT $1, defaults.key, defaults.enabled, 'lazy default'
        FROM (VALUES
            ('ticket_sales_enabled', true),
            ('ticket_delivery_enabled', true),
            ('gate_redemption_enabled', true),
            ('mailer_enabled', true),
            ('meta_publish_enabled', true),
            ('bandsintown_sync_enabled', true),
            ('n8n_ingress_enabled', true),
            ('automatic_retry_enabled', true),
            ('draw_proofs_enabled', true),
            ('external_proof_anchoring_enabled', false),
            ('merch_inventory_enabled', false),
            ('reward_campaigns_enabled', false),
            ('merch_inventory_writes_enabled', false)
        ) AS defaults(key, enabled)
        ON CONFLICT (workspace_id, key) DO NOTHING
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .execute(state.ticketing.pool())
    .await
    .map_err(EcosystemError::sqlx)?;
    Ok(())
}

async fn load_flags(state: &crate::AppState) -> Result<Vec<FeatureFlag>, EcosystemError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let flags = sqlx::query_as::<_, FeatureFlag>(
        r#"
        SELECT key, enabled, reason, version, updated_at
        FROM ecosystem_feature_flags
        WHERE workspace_id = $1
        ORDER BY key
        "#,
    )
    .bind(workspace_id)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(EcosystemError::sqlx)?;
    for flag in &flags {
        if let Some((key, _)) = flag_definition(&flag.key) {
            write_cached_flag(workspace_id, key, flag.enabled).await;
        }
    }
    Ok(flags)
}

async fn load_last_reconciliation(
    state: &crate::AppState,
) -> Result<Option<ReconciliationRun>, EcosystemError> {
    sqlx::query_as::<_, ReconciliationRun>(
        r#"
        SELECT id, status, trigger, finding_count, started_at, finished_at
        FROM reconciliation_runs
        WHERE workspace_id = $1
        ORDER BY started_at DESC, id DESC
        LIMIT 1
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(EcosystemError::sqlx)
}

async fn count_open_findings(state: &crate::AppState) -> Result<i64, EcosystemError> {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM reconciliation_findings WHERE workspace_id = $1 AND resolved_at IS NULL",
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_one(state.ticketing.pool())
    .await
    .map_err(EcosystemError::sqlx)
}

async fn load_next_event(state: &crate::AppState) -> Result<Option<OverviewEvent>, EcosystemError> {
    sqlx::query_as::<_, OverviewEvent>(
        r#"
        SELECT id, slug, title, venue, starts_at
        FROM events
        WHERE workspace_id = $1 AND status = 'published'
          AND starts_at >= now() - interval '6 hours'
        ORDER BY starts_at, id
        LIMIT 1
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(EcosystemError::sqlx)
}

async fn read_cached_flag(workspace_id: Uuid, key: &'static str) -> Option<bool> {
    let now = Instant::now();
    flag_cache()
        .read()
        .await
        .get(&(workspace_id, key))
        .filter(|entry| entry.expires_at > now)
        .map(|entry| entry.enabled)
}

fn insert_cached_flag(
    cache: &mut FlagCache,
    workspace_id: Uuid,
    key: &'static str,
    enabled: bool,
    now: Instant,
) {
    let cache_key = (workspace_id, key);
    if cache.len() >= MAX_FLAG_CACHE_ENTRIES && !cache.contains_key(&cache_key) {
        cache.retain(|_, entry| entry.expires_at > now);
        if cache.len() >= MAX_FLAG_CACHE_ENTRIES
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(key, _)| *key)
        {
            cache.remove(&oldest);
        }
    }
    cache.insert(
        cache_key,
        CachedFlag {
            enabled,
            expires_at: now + FLAG_CACHE_TTL,
        },
    );
}

async fn write_cached_flag(workspace_id: Uuid, key: &'static str, enabled: bool) {
    let now = Instant::now();
    let mut cache = flag_cache().write().await;
    insert_cached_flag(&mut cache, workspace_id, key, enabled, now);
}

pub(crate) async fn cache_feature_flag(workspace_id: Uuid, key: &'static str, enabled: bool) {
    write_cached_flag(workspace_id, key, enabled).await;
}

async fn update_flag_inner(
    state: &crate::AppState,
    headers: &HeaderMap,
    key: &str,
    payload: UpdateFlagRequest,
) -> Result<FlagMutationResult, EcosystemError> {
    if flag_default(key).is_none()
        || payload
            .reason
            .as_deref()
            .is_some_and(|reason| reason.trim().len() > 500)
    {
        return Err(EcosystemError::BadRequest);
    }
    let idempotency_key = mutation_key(headers)?;
    let request_hash =
        hash_json(&json!({"key": key, "enabled": payload.enabled, "reason": payload.reason}));
    let target_id = deterministic_id("feature_flag", key);
    let mut tx = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(EcosystemError::sqlx)?;
    configure_transaction(&mut tx, state).await?;
    lock_mutation(&mut tx, state, &idempotency_key).await?;
    if let Some(existing) = existing_mutation(&mut tx, state, &idempotency_key).await? {
        validate_replay(
            &existing,
            "feature_flag.updated",
            "feature_flag",
            target_id,
            &request_hash,
        )?;
        let flag = load_flag_tx(&mut tx, state, key).await?;
        tx.commit().await.map_err(EcosystemError::sqlx)?;
        if let Some((key, _)) = flag_definition(key) {
            write_cached_flag(
                state.ticketing.workspace_id().into_uuid(),
                key,
                flag.enabled,
            )
            .await;
        }
        return Ok(FlagMutationResult {
            flag,
            replayed: true,
        });
    }
    sqlx::query(
        r#"
        INSERT INTO ecosystem_feature_flags (
            workspace_id, key, enabled, reason, updated_by_request_id
        ) VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (workspace_id, key) DO UPDATE
        SET enabled = EXCLUDED.enabled,
            reason = EXCLUDED.reason,
            version = ecosystem_feature_flags.version + 1,
            updated_at = now(),
            updated_by_request_id = EXCLUDED.updated_by_request_id
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(key)
    .bind(payload.enabled)
    .bind(
        payload
            .reason
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty()),
    )
    .bind(request_id(headers))
    .execute(&mut *tx)
    .await
    .map_err(EcosystemError::sqlx)?;
    append_action(
        &mut tx,
        state,
        "feature_flag.updated",
        "feature_flag",
        target_id,
        &idempotency_key,
        request_id(headers).as_deref(),
        &request_hash,
        json!({"key": key, "enabled": payload.enabled}),
    )
    .await?;
    let flag = load_flag_tx(&mut tx, state, key).await?;
    tx.commit().await.map_err(EcosystemError::sqlx)?;
    if let Some((key, _)) = flag_definition(key) {
        write_cached_flag(
            state.ticketing.workspace_id().into_uuid(),
            key,
            flag.enabled,
        )
        .await;
    }
    Ok(FlagMutationResult {
        flag,
        replayed: false,
    })
}

async fn reconcile_inner(
    state: &crate::AppState,
    headers: &HeaderMap,
    trigger: &str,
) -> Result<ReconciliationResult, EcosystemError> {
    if !matches!(trigger, "manual" | "scheduled" | "deploy" | "restore_drill") {
        return Err(EcosystemError::BadRequest);
    }
    let idempotency_key = mutation_key(headers)?;
    let request_hash = hash_json(&json!({"trigger": trigger}));
    let target_id = deterministic_id("reconciliation", &idempotency_key);
    let mut tx = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(EcosystemError::sqlx)?;
    configure_transaction(&mut tx, state).await?;
    lock_mutation(&mut tx, state, &idempotency_key).await?;
    if let Some(existing) = existing_mutation(&mut tx, state, &idempotency_key).await? {
        validate_replay(
            &existing,
            "reconciliation.run",
            "reconciliation",
            target_id,
            &request_hash,
        )?;
        let run_id = existing
            .details
            .get("run_id")
            .and_then(Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or(EcosystemError::Conflict)?;
        let result = load_reconciliation_tx(&mut tx, state, run_id).await?;
        tx.commit().await.map_err(EcosystemError::sqlx)?;
        return Ok(ReconciliationResult {
            replayed: true,
            ..result
        });
    }

    let run_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO reconciliation_runs (id, workspace_id, status, trigger, request_id)
        VALUES ($1, $2, 'running', $3, $4)
        "#,
    )
    .bind(run_id)
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(trigger)
    .bind(request_id(headers))
    .execute(&mut *tx)
    .await
    .map_err(EcosystemError::sqlx)?;

    insert_reconciliation_findings(&mut tx, state, run_id).await?;
    let finding_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM reconciliation_findings WHERE workspace_id = $1 AND run_id = $2",
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(run_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(EcosystemError::sqlx)?;
    let finding_count_i32 = i32::try_from(finding_count).map_err(|_| EcosystemError::Unexpected)?;
    sqlx::query(
        r#"
        UPDATE reconciliation_runs
        SET status = 'completed', finding_count = $3, finished_at = now()
        WHERE workspace_id = $1 AND id = $2 AND status = 'running'
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(run_id)
    .bind(finding_count_i32)
    .execute(&mut *tx)
    .await
    .map_err(EcosystemError::sqlx)?;

    sqlx::query(
        r#"
        INSERT INTO outbox_events (workspace_id, event_type, event_version, payload, request_id)
        SELECT finding.workspace_id,
               'reconciliation.finding_raised',
               1,
               jsonb_build_object(
                   'finding_id', finding.id,
                   'run_id', finding.run_id,
                   'kind', finding.kind,
                   'severity', finding.severity,
                   'entity_id', finding.entity_id,
                   'entity_label', finding.entity_label,
                   'summary', finding.summary,
                   'suggested_action', finding.suggested_action
               ),
               $3
        FROM reconciliation_findings AS finding
        WHERE finding.workspace_id = $1 AND finding.run_id = $2
          AND finding.severity IN ('warning', 'critical')
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(run_id)
    .bind(request_id(headers))
    .execute(&mut *tx)
    .await
    .map_err(EcosystemError::sqlx)?;

    append_action(
        &mut tx,
        state,
        "reconciliation.run",
        "reconciliation",
        target_id,
        &idempotency_key,
        request_id(headers).as_deref(),
        &request_hash,
        json!({"run_id": run_id, "finding_count": finding_count_i32, "trigger": trigger}),
    )
    .await?;
    let result = load_reconciliation_tx(&mut tx, state, run_id).await?;
    tx.commit().await.map_err(EcosystemError::sqlx)?;
    Ok(ReconciliationResult {
        replayed: false,
        ..result
    })
}

async fn insert_reconciliation_findings(
    tx: &mut Transaction<'_, Postgres>,
    state: &crate::AppState,
    run_id: Uuid,
) -> Result<(), EcosystemError> {
    sqlx::query(
        r#"
        INSERT INTO reconciliation_findings (
            workspace_id, run_id, kind, severity, entity_type, entity_id,
            entity_label, summary, suggested_action, metadata
        )
        SELECT $1, $2, 'ticket.pass_count_mismatch', 'critical', 'ticket_order',
               ticket_order.id, ticket_order.public_reference,
               'Paid ticket order does not have the expected number of admission passes',
               'inspect_ticket_order',
               jsonb_build_object('expected', expected.quantity, 'actual', actual.quantity)
        FROM ticket_orders AS ticket_order
        JOIN LATERAL (
            SELECT COALESCE(sum(item.quantity), 0)::bigint AS quantity
            FROM ticket_order_items AS item
            WHERE item.workspace_id = ticket_order.workspace_id
              AND item.ticket_order_id = ticket_order.id
        ) AS expected ON true
        JOIN LATERAL (
            SELECT count(pass.id)::bigint AS quantity
            FROM admission_passes AS pass
            JOIN ticket_order_items AS item
              ON item.workspace_id = pass.workspace_id
             AND item.id = pass.ticket_order_item_id
            WHERE item.workspace_id = ticket_order.workspace_id
              AND item.ticket_order_id = ticket_order.id
              AND pass.issuance_method = 'paid'
        ) AS actual ON true
        WHERE ticket_order.workspace_id = $1
          AND ticket_order.status IN ('paid', 'partially_refunded', 'refunded')
          AND expected.quantity <> actual.quantity

        UNION ALL

        SELECT $1, $2, 'ticket.paid_event_missing', 'warning', 'ticket_order',
               ticket_order.id, ticket_order.public_reference,
               'Paid ticket order has no durable ticket.order.paid outbox event',
               'inspect_outbox', '{}'::jsonb
        FROM ticket_orders AS ticket_order
        WHERE ticket_order.workspace_id = $1
          AND ticket_order.status IN ('paid', 'partially_refunded', 'refunded')
          AND NOT EXISTS (
              SELECT 1 FROM outbox_events AS event
              WHERE event.workspace_id = ticket_order.workspace_id
                AND event.event_type = 'ticket.order.paid'
                AND event.payload ->> 'order_id' = ticket_order.id::text
          )

        UNION ALL

        SELECT $1, $2, 'ticket.delivery_event_missing', 'warning', 'ticket_order',
               request.ticket_order_id, ticket_order.public_reference,
               'Ticket delivery request has no matching durable outbox event',
               'request_delivery_retry',
               jsonb_build_object('delivery_request_id', request.id)
        FROM ticket_delivery_requests AS request
        JOIN ticket_orders AS ticket_order
          ON ticket_order.workspace_id = request.workspace_id
         AND ticket_order.id = request.ticket_order_id
        WHERE request.workspace_id = $1
          AND NOT EXISTS (
              SELECT 1 FROM outbox_events AS event
              WHERE event.workspace_id = request.workspace_id
                AND event.event_type = 'ticket.order.delivery_requested'
                AND event.payload ->> 'order_id' = request.ticket_order_id::text
                AND event.created_at >= request.created_at - interval '5 seconds'
          )

        UNION ALL

        SELECT $1, $2, 'outbox.dead', 'critical', 'outbox_event', event.id,
               event.event_type,
               'Outbox event exhausted automatic retries', 'retry_outbox',
               jsonb_build_object('attempts', event.attempts, 'error_kind', event.last_error_kind)
        FROM outbox_events AS event
        WHERE event.workspace_id = $1 AND event.status = 'dead'

        UNION ALL

        SELECT $1, $2, 'webhook.dead', 'critical', 'webhook_delivery', delivery.id,
               endpoint.name,
               'Webhook delivery exhausted automatic retries', 'retry_delivery',
               jsonb_build_object(
                   'attempts', delivery.attempt_count,
                   'error_kind', delivery.last_error_kind,
                   'endpoint_active', endpoint.active
               )
        FROM webhook_deliveries AS delivery
        JOIN webhook_endpoints AS endpoint
          ON endpoint.workspace_id = delivery.workspace_id
         AND endpoint.id = delivery.endpoint_id
        WHERE delivery.workspace_id = $1 AND delivery.status = 'dead'
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(run_id)
    .execute(&mut **tx)
    .await
    .map_err(EcosystemError::sqlx)?;
    Ok(())
}

async fn load_reconciliation_tx(
    tx: &mut Transaction<'_, Postgres>,
    state: &crate::AppState,
    run_id: Uuid,
) -> Result<ReconciliationResult, EcosystemError> {
    let run = sqlx::query_as::<_, ReconciliationRun>(
        r#"
        SELECT id, status, trigger, finding_count, started_at, finished_at
        FROM reconciliation_runs
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(EcosystemError::sqlx)?;
    let findings = sqlx::query_as::<_, ReconciliationFinding>(
        r#"
        SELECT id, run_id, kind, severity, entity_type, entity_id,
               entity_label, summary, suggested_action, metadata,
               created_at, resolved_at
        FROM reconciliation_findings
        WHERE workspace_id = $1 AND run_id = $2
        ORDER BY severity DESC, created_at, id
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(run_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(EcosystemError::sqlx)?;
    Ok(ReconciliationResult {
        run,
        findings,
        replayed: false,
    })
}

async fn load_checklist(
    state: &crate::AppState,
    event_slug: &str,
) -> Result<ShowChecklist, EcosystemError> {
    let event = sqlx::query_as::<_, OverviewEvent>(
        r#"
        SELECT id, slug, title, venue, starts_at
        FROM events
        WHERE workspace_id = $1 AND slug = $2
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(event_slug)
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(EcosystemError::sqlx)?
    .ok_or(EcosystemError::NotFound)?;
    ensure_checklist_defaults(state, event.id).await?;
    let items = sqlx::query_as::<_, ChecklistItem>(
        r#"
        SELECT item_key, status, note, updated_at
        FROM show_checklist_items
        WHERE workspace_id = $1 AND event_id = $2
        ORDER BY item_key
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(event.id)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(EcosystemError::sqlx)?;
    Ok(ShowChecklist {
        event_id: event.id,
        event_slug: event.slug,
        event_title: event.title,
        starts_at: event.starts_at,
        items,
    })
}

async fn ensure_checklist_defaults(
    state: &crate::AppState,
    event_id: Uuid,
) -> Result<(), EcosystemError> {
    sqlx::query(
        r#"
        INSERT INTO show_checklist_items (workspace_id, event_id, item_key, status)
        SELECT $1, $2, defaults.item_key, 'pending'
        FROM (VALUES
            ('announcement_published'),
            ('ticketing_verified'),
            ('staff_assigned'),
            ('offline_snapshot_ready'),
            ('gate_device_charged'),
            ('backup_device_ready'),
            ('network_tested'),
            ('guestlist_checked'),
            ('post_show_reconciliation'),
            ('post_show_report')
        ) AS defaults(item_key)
        ON CONFLICT (workspace_id, event_id, item_key) DO NOTHING
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(event_id)
    .execute(state.ticketing.pool())
    .await
    .map_err(EcosystemError::sqlx)?;
    Ok(())
}

async fn update_checklist_inner(
    state: &crate::AppState,
    headers: &HeaderMap,
    event_slug: &str,
    item_key: &str,
    payload: UpdateChecklistRequest,
) -> Result<ShowChecklist, EcosystemError> {
    if !matches!(
        payload.status.as_str(),
        "pending" | "done" | "blocked" | "skipped"
    ) || item_key.is_empty()
        || item_key.len() > 64
        || payload
            .note
            .as_deref()
            .is_some_and(|note| note.len() > 1000)
    {
        return Err(EcosystemError::BadRequest);
    }
    let idempotency_key = mutation_key(headers)?;
    let request_hash = hash_json(&json!({
        "event_slug": event_slug,
        "item_key": item_key,
        "status": payload.status,
        "note": payload.note,
    }));
    let event = sqlx::query_as::<_, OverviewEvent>(
        "SELECT id, slug, title, venue, starts_at FROM events WHERE workspace_id = $1 AND slug = $2",
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(event_slug)
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(EcosystemError::sqlx)?
    .ok_or(EcosystemError::NotFound)?;
    let target_id = deterministic_id("checklist", &format!("{}:{item_key}", event.id));
    let mut tx = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(EcosystemError::sqlx)?;
    configure_transaction(&mut tx, state).await?;
    lock_mutation(&mut tx, state, &idempotency_key).await?;
    if let Some(existing) = existing_mutation(&mut tx, state, &idempotency_key).await? {
        validate_replay(
            &existing,
            "show_checklist.updated",
            "show_checklist",
            target_id,
            &request_hash,
        )?;
        tx.commit().await.map_err(EcosystemError::sqlx)?;
        return load_checklist(state, event_slug).await;
    }
    sqlx::query(
        r#"
        INSERT INTO show_checklist_items (
            workspace_id, event_id, item_key, status, note, updated_by_request_id
        ) VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (workspace_id, event_id, item_key) DO UPDATE
        SET status = EXCLUDED.status,
            note = EXCLUDED.note,
            updated_at = now(),
            updated_by_request_id = EXCLUDED.updated_by_request_id
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(event.id)
    .bind(item_key)
    .bind(payload.status)
    .bind(
        payload
            .note
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty()),
    )
    .bind(request_id(headers))
    .execute(&mut *tx)
    .await
    .map_err(EcosystemError::sqlx)?;
    append_action(
        &mut tx,
        state,
        "show_checklist.updated",
        "show_checklist",
        target_id,
        &idempotency_key,
        request_id(headers).as_deref(),
        &request_hash,
        json!({"event_id": event.id, "item_key": item_key}),
    )
    .await?;
    tx.commit().await.map_err(EcosystemError::sqlx)?;
    load_checklist(state, event_slug).await
}

async fn emit_due_inner(
    state: &crate::AppState,
    request_id_value: Option<&str>,
) -> Result<EmissionResult, EcosystemError> {
    let mut tx = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(EcosystemError::sqlx)?;
    configure_transaction(&mut tx, state).await?;
    let emitted = sqlx::query_scalar::<_, i64>(
        r#"
        WITH due AS (
            SELECT event.id AS event_id, event.title, event.starts_at,
                   CASE
                       WHEN event.starts_at BETWEEN now() + interval '6 days' AND now() + interval '8 days' THEN 'week'
                       WHEN event.starts_at BETWEEN now() + interval '18 hours' AND now() + interval '30 hours' THEN 'day'
                       WHEN event.starts_at BETWEEN now() + interval '90 minutes' AND now() + interval '3 hours' THEN 'gate'
                       WHEN event.starts_at BETWEEN now() - interval '8 hours' AND now() - interval '1 hour' THEN 'followup'
                   END AS phase
            FROM events AS event
            WHERE event.workspace_id = $1
              AND event.status IN ('published', 'completed')
        ), inserted_events AS (
            INSERT INTO outbox_events (workspace_id, event_type, event_version, payload, request_id)
            SELECT $1,
                   CASE WHEN due.phase = 'followup' THEN 'show.followup_due' ELSE 'show.checklist_due' END,
                   1,
                   jsonb_build_object(
                       'event_id', due.event_id,
                       'event_title', due.title,
                       'starts_at', due.starts_at,
                       'checklist', due.phase,
                       'severity', CASE WHEN due.phase = 'gate' THEN 'warning' ELSE 'info' END,
                       'summary', CASE due.phase
                           WHEN 'week' THEN 'Tydzień do koncertu: domknij sprzedaż, komunikację i obsadę.'
                           WHEN 'day' THEN 'Dzień do koncertu: pobierz snapshot offline i sprawdź guestlistę.'
                           WHEN 'gate' THEN 'Bramka zaraz rusza: urządzenia, backup i sieć muszą być gotowe.'
                           ELSE 'Po koncercie: uruchom reconciliation i raport wydarzenia.'
                       END
                   ),
                   $2
            FROM due
            WHERE due.phase IS NOT NULL
              AND NOT EXISTS (
                  SELECT 1 FROM show_notification_emissions AS emission
                  WHERE emission.workspace_id = $1
                    AND emission.event_id = due.event_id
                    AND emission.phase = due.phase
              )
            RETURNING id, payload
        ), emissions AS (
            INSERT INTO show_notification_emissions (
                workspace_id, event_id, phase, outbox_event_id
            )
            SELECT $1,
                   (payload ->> 'event_id')::uuid,
                   payload ->> 'checklist',
                   id
            FROM inserted_events
            RETURNING 1
        )
        SELECT count(*)::bigint FROM emissions
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(request_id_value)
    .fetch_one(&mut *tx)
    .await
    .map_err(EcosystemError::sqlx)?;
    tx.commit().await.map_err(EcosystemError::sqlx)?;
    Ok(EmissionResult { emitted })
}

async fn load_show_snapshot(
    state: &crate::AppState,
    event_slug: &str,
) -> Result<ShowModeSnapshot, EcosystemError> {
    let signing_key = state
        .ticketing
        .checkout_token_key()
        .ok_or(EcosystemError::Unavailable)?;
    let event = sqlx::query_as::<_, ShowEventRow>(
        r#"
        SELECT id, slug, title, venue, starts_at, doors_at, ends_at
        FROM events
        WHERE workspace_id = $1 AND slug = $2 AND status IN ('published', 'completed')
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(event_slug)
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(EcosystemError::sqlx)?
    .ok_or(EcosystemError::NotFound)?;
    let rows = sqlx::query_as::<_, ShowPassRow>(
        r#"
        SELECT pass.id AS pass_id, pass.public_reference, pass.holder_name,
               pass.holder_email, pass.issuance_method, pass.status,
               ticket_type.name AS ticket_type_name
        FROM admission_passes AS pass
        LEFT JOIN ticket_order_items AS item
          ON item.workspace_id = pass.workspace_id AND item.id = pass.ticket_order_item_id
        LEFT JOIN ticket_types AS ticket_type
          ON ticket_type.workspace_id = item.workspace_id AND ticket_type.id = item.ticket_type_id
        WHERE pass.workspace_id = $1 AND pass.event_id = $2
          AND pass.status IN ('claimed', 'redeemed')
        ORDER BY pass.public_reference
        LIMIT $3
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(event.id)
    .bind(MAX_SHOW_PASSES + 1)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(EcosystemError::sqlx)?;
    if i64::try_from(rows.len()).map_err(|_| EcosystemError::Unexpected)? > MAX_SHOW_PASSES {
        return Err(EcosystemError::Conflict);
    }
    let generated_at = OffsetDateTime::now_utc();
    let qr_not_before = event
        .doors_at
        .unwrap_or(event.starts_at)
        .saturating_sub(TimeDuration::hours(6));
    let qr_expires_at = event
        .ends_at
        .unwrap_or_else(|| event.starts_at.saturating_add(TimeDuration::hours(12)))
        .checked_add(TimeDuration::hours(24))
        .ok_or(EcosystemError::Unexpected)?;
    let expires_at = std::cmp::min(
        qr_expires_at,
        generated_at.saturating_add(TimeDuration::hours(48)),
    );
    if expires_at <= generated_at {
        return Err(EcosystemError::Conflict);
    }
    let mut passes = Vec::with_capacity(rows.len());
    for row in rows {
        let offline_eligible = row.issuance_method == "paid" && row.status == "claimed";
        let qr_sha256 = if offline_eligible {
            let token = encode_ticket_qr(
                row.pass_id,
                event.id,
                &row.public_reference,
                qr_not_before.unix_timestamp(),
                qr_expires_at.unix_timestamp(),
                &signing_key,
            )
            .map_err(|_| EcosystemError::Unexpected)?;
            Some(hex::encode(Sha256::digest(token.as_bytes())))
        } else {
            None
        };
        passes.push(ShowModePass {
            public_reference: row.public_reference,
            holder_name: row.holder_name,
            holder_email_masked: mask_email(row.holder_email.as_deref()),
            ticket_type_name: row.ticket_type_name,
            offline_eligible,
            qr_sha256,
        });
    }
    let event_view = ShowModeEvent {
        slug: event.slug,
        title: event.title,
        venue: event.venue,
        starts_at: format_time(event.starts_at)?,
    };
    let generated_at_text = format_time(generated_at)?;
    let expires_at_text = format_time(expires_at)?;
    let snapshot_id = Uuid::now_v7().to_string();
    let checksum_sha256 = snapshot_checksum(
        &snapshot_id,
        &event_view,
        &generated_at_text,
        &expires_at_text,
        &passes,
    );
    Ok(ShowModeSnapshot {
        schema_version: SHOW_SNAPSHOT_SCHEMA,
        snapshot_id,
        event: event_view,
        generated_at: generated_at_text,
        expires_at: expires_at_text,
        checksum_sha256,
        passes,
    })
}

fn snapshot_checksum(
    snapshot_id: &str,
    event: &ShowModeEvent,
    generated_at: &str,
    expires_at: &str,
    passes: &[ShowModePass],
) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, "crowdrelay/show-mode/v1");
    hash_field(&mut hasher, &SHOW_SNAPSHOT_SCHEMA.to_string());
    hash_field(&mut hasher, snapshot_id);
    hash_field(&mut hasher, &event.slug);
    hash_field(&mut hasher, &event.title);
    hash_field(&mut hasher, event.venue.as_deref().unwrap_or(""));
    hash_field(&mut hasher, &event.starts_at);
    hash_field(&mut hasher, generated_at);
    hash_field(&mut hasher, expires_at);
    // The snapshot query orders by public_reference, so hashing can stream
    // directly without a second 10k-entry allocation and O(n log n) sort.
    for pass in passes {
        hash_field(&mut hasher, &pass.public_reference);
        hash_field(&mut hasher, pass.holder_name.as_deref().unwrap_or(""));
        hash_field(&mut hasher, &pass.holder_email_masked);
        hash_field(&mut hasher, pass.ticket_type_name.as_deref().unwrap_or(""));
        hash_field(&mut hasher, if pass.offline_eligible { "1" } else { "0" });
        hash_field(&mut hasher, pass.qr_sha256.as_deref().unwrap_or(""));
    }
    hex::encode(hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, value: &str) {
    hasher.update(value.len().to_be_bytes());
    hasher.update(value.as_bytes());
}

fn format_time(value: OffsetDateTime) -> Result<String, EcosystemError> {
    value
        .format(&Rfc3339)
        .map_err(|_| EcosystemError::Unexpected)
}

fn mask_email(value: Option<&str>) -> String {
    let Some((local, domain)) = value.and_then(|email| email.split_once('@')) else {
        return "—".to_owned();
    };
    let prefix = local.chars().next().unwrap_or('*');
    format!("{prefix}***@{domain}")
}

fn flag_definition(key: &str) -> Option<(&'static str, bool)> {
    FLAG_KEYS
        .iter()
        .find_map(|(candidate, enabled)| (*candidate == key).then_some((*candidate, *enabled)))
}

fn flag_default(key: &str) -> Option<bool> {
    flag_definition(key).map(|(_, enabled)| enabled)
}

fn mutation_key(headers: &HeaderMap) -> Result<String, EcosystemError> {
    headers
        .get(&IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| {
            (8..=128).contains(&value.len())
                && value.bytes().all(|byte| (b'!'..=b'~').contains(&byte))
        })
        .map(str::to_owned)
        .ok_or(EcosystemError::BadRequest)
}

fn hash_json(value: &Value) -> String {
    hex::encode(Sha256::digest(value.to_string().as_bytes()))
}

fn deterministic_id(namespace: &str, value: &str) -> Uuid {
    let mut digest = Sha256::new();
    digest.update(namespace.as_bytes());
    digest.update([0]);
    digest.update(value.as_bytes());
    let bytes: [u8; 32] = digest.finalize().into();
    let mut id = [0_u8; 16];
    id.copy_from_slice(&bytes[..16]);
    id[6] = (id[6] & 0x0f) | 0x80;
    id[8] = (id[8] & 0x3f) | 0x80;
    Uuid::from_bytes(id)
}

async fn configure_transaction(
    tx: &mut Transaction<'_, Postgres>,
    state: &crate::AppState,
) -> Result<(), EcosystemError> {
    let statement_ms = state.ticketing.operation_timeout().as_millis();
    let lock_ms = state.ticketing.lock_timeout().as_millis();
    if statement_ms == 0
        || lock_ms == 0
        || statement_ms > i32::MAX as u128
        || lock_ms > i32::MAX as u128
    {
        return Err(EcosystemError::Unexpected);
    }
    sqlx::query(
        "SELECT set_config('statement_timeout', $1, true), set_config('lock_timeout', $2, true)",
    )
    .bind(format!("{statement_ms}ms"))
    .bind(format!("{lock_ms}ms"))
    .execute(&mut **tx)
    .await
    .map_err(EcosystemError::sqlx)?;
    Ok(())
}

async fn lock_mutation(
    tx: &mut Transaction<'_, Postgres>,
    state: &crate::AppState,
    idempotency_key: &str,
) -> Result<(), EcosystemError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, hashtextextended($2, 0)))")
        .bind(state.ticketing.workspace_id().into_uuid().to_string())
        .bind(idempotency_key)
        .execute(&mut **tx)
        .await
        .map_err(EcosystemError::sqlx)?;
    Ok(())
}

async fn existing_mutation(
    tx: &mut Transaction<'_, Postgres>,
    state: &crate::AppState,
    idempotency_key: &str,
) -> Result<Option<ExistingMutation>, EcosystemError> {
    sqlx::query_as::<_, ExistingMutation>(
        r#"
        SELECT action, target_type, target_id, details
        FROM operator_actions
        WHERE workspace_id = $1 AND idempotency_key = $2
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(idempotency_key)
    .fetch_optional(&mut **tx)
    .await
    .map_err(EcosystemError::sqlx)
}

fn validate_replay(
    existing: &ExistingMutation,
    action: &str,
    target_type: &str,
    target_id: Uuid,
    request_hash: &str,
) -> Result<(), EcosystemError> {
    let existing_hash = existing.details.get("request_hash").and_then(Value::as_str);
    if existing.action == action
        && existing.target_type == target_type
        && existing.target_id == target_id
        && existing_hash == Some(request_hash)
    {
        Ok(())
    } else {
        Err(EcosystemError::Conflict)
    }
}

#[allow(clippy::too_many_arguments)]
async fn append_action(
    tx: &mut Transaction<'_, Postgres>,
    state: &crate::AppState,
    action: &str,
    target_type: &str,
    target_id: Uuid,
    idempotency_key: &str,
    request_id_value: Option<&str>,
    request_hash: &str,
    mut details: Value,
) -> Result<(), EcosystemError> {
    let object = details.as_object_mut().ok_or(EcosystemError::Unexpected)?;
    object.insert(
        "request_hash".to_owned(),
        Value::String(request_hash.to_owned()),
    );
    sqlx::query(
        r#"
        INSERT INTO operator_actions (
            workspace_id, action, target_type, target_id,
            idempotency_key, request_id, details
        ) VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(action)
    .bind(target_type)
    .bind(target_id)
    .bind(idempotency_key)
    .bind(request_id_value)
    .bind(details)
    .execute(&mut **tx)
    .await
    .map_err(EcosystemError::sqlx)?;
    Ok(())
}

async fn load_flag_tx(
    tx: &mut Transaction<'_, Postgres>,
    state: &crate::AppState,
    key: &str,
) -> Result<FeatureFlag, EcosystemError> {
    sqlx::query_as::<_, FeatureFlag>(
        "SELECT key, enabled, reason, version, updated_at FROM ecosystem_feature_flags WHERE workspace_id = $1 AND key = $2",
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(key)
    .fetch_one(&mut **tx)
    .await
    .map_err(EcosystemError::sqlx)
}

async fn run<T>(
    state: &crate::AppState,
    future: impl Future<Output = Result<T, EcosystemError>>,
) -> Result<T, EcosystemError> {
    timeout(state.ticketing.operation_timeout(), future)
        .await
        .map_err(|_| EcosystemError::Unavailable)?
}

fn respond<T: Serialize>(
    result: Result<T, EcosystemError>,
    request_id_value: Option<String>,
) -> Response {
    match result {
        Ok(value) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(value),
        )
            .into_response(),
        Err(error) => error.into_response(request_id_value),
    }
}

#[derive(Debug)]
pub(crate) enum EcosystemError {
    BadRequest,
    NotFound,
    Conflict,
    Unavailable,
    Unexpected,
}

impl EcosystemError {
    fn sqlx(error: sqlx::Error) -> Self {
        tracing::error!(error = %error, "ecosystem control-plane query failed");
        Self::Unexpected
    }

    pub(crate) fn into_response(self, request_id_value: Option<String>) -> Response {
        match self {
            Self::BadRequest => Problem::bad_request(request_id_value)
                .private()
                .into_response(),
            Self::NotFound => Problem::not_found(request_id_value)
                .private()
                .into_response(),
            Self::Conflict => Problem::conflict(request_id_value)
                .private()
                .into_response(),
            Self::Unavailable => Problem::service_unavailable(request_id_value)
                .private()
                .into_response(),
            Self::Unexpected => Problem::internal(request_id_value)
                .private()
                .into_response(),
        }
    }
}

#[cfg(test)]
mod basic_tests {
    use super::{deterministic_id, flag_default, hash_json};
    use serde_json::json;

    #[test]
    fn identifiers_and_request_hashes_are_stable() {
        assert_eq!(deterministic_id("flag", "x"), deterministic_id("flag", "x"));
        assert_ne!(deterministic_id("flag", "x"), deterministic_id("flag", "y"));
        assert_eq!(hash_json(&json!({"a": 1})), hash_json(&json!({"a": 1})));
    }

    #[test]
    fn only_known_feature_flags_are_addressable() {
        assert_eq!(flag_default("ticket_sales_enabled"), Some(true));
        assert_eq!(flag_default("merch_inventory_enabled"), Some(false));
        assert_eq!(flag_default("reward_campaigns_enabled"), Some(false));
        assert_eq!(flag_default("merch_inventory_writes_enabled"), Some(false));
        assert_eq!(flag_default("unknown"), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event() -> ShowModeEvent {
        ShowModeEvent {
            slug: "virya-live".to_owned(),
            title: "Virya Live".to_owned(),
            venue: Some("Club".to_owned()),
            starts_at: "2026-08-02T18:00:00Z".to_owned(),
        }
    }

    fn sample_pass() -> ShowModePass {
        ShowModePass {
            public_reference: "VRY-TICKET-1".to_owned(),
            holder_name: Some("Fan".to_owned()),
            holder_email_masked: "f***@example.com".to_owned(),
            ticket_type_name: Some("Regular".to_owned()),
            offline_eligible: true,
            qr_sha256: Some("ab".repeat(32)),
        }
    }

    #[test]
    fn snapshot_checksum_is_stable_and_sensitive() {
        let event = sample_event();
        let pass = sample_pass();
        let checksum = snapshot_checksum(
            "snapshot-1",
            &event,
            "generated",
            "expires",
            std::slice::from_ref(&pass),
        );
        assert_eq!(
            checksum,
            snapshot_checksum(
                "snapshot-1",
                &event,
                "generated",
                "expires",
                std::slice::from_ref(&pass)
            )
        );
        let mut changed = pass;
        changed.offline_eligible = false;
        assert_ne!(
            checksum,
            snapshot_checksum("snapshot-1", &event, "generated", "expires", &[changed])
        );
    }

    #[test]
    fn deterministic_ids_are_namespaced_and_uuid_v8() {
        let first = deterministic_id("flag", "ticket_sales_enabled");
        assert_eq!(first, deterministic_id("flag", "ticket_sales_enabled"));
        assert_ne!(first, deterministic_id("checklist", "ticket_sales_enabled"));
        assert_eq!(first.get_version_num(), 8);
    }

    #[test]
    fn mutation_keys_reject_whitespace_and_control_bytes() {
        let mut headers = HeaderMap::new();
        headers.insert(IDEMPOTENCY_KEY.clone(), "valid-key-123".parse().unwrap());
        assert_eq!(mutation_key(&headers).unwrap(), "valid-key-123");
        headers.insert(IDEMPOTENCY_KEY.clone(), "bad key 123".parse().unwrap());
        assert!(matches!(
            mutation_key(&headers),
            Err(EcosystemError::BadRequest)
        ));
    }

    #[test]
    fn email_masking_never_exposes_the_local_part() {
        assert_eq!(mask_email(Some("wojciech@example.com")), "w***@example.com");
        assert_eq!(mask_email(Some("invalid")), "—");
        assert_eq!(mask_email(None), "—");
    }

    #[test]
    fn all_expected_feature_flags_have_safe_defaults() {
        assert_eq!(FLAG_KEYS.len(), 13);
        assert_eq!(flag_default("ticket_sales_enabled"), Some(true));
        assert_eq!(flag_default("unknown"), None);
    }

    #[test]
    fn feature_flag_cache_is_strictly_bounded() {
        let now = Instant::now();
        let mut cache = FlagCache::new();
        for index in 0..(MAX_FLAG_CACHE_ENTRIES + 32) {
            insert_cached_flag(
                &mut cache,
                Uuid::from_u128(index as u128 + 1),
                "mailer_enabled",
                index % 2 == 0,
                now,
            );
        }
        assert_eq!(cache.len(), MAX_FLAG_CACHE_ENTRIES);
        assert!(cache.contains_key(&(
            Uuid::from_u128((MAX_FLAG_CACHE_ENTRIES + 32) as u128),
            "mailer_enabled"
        )));
    }
}
