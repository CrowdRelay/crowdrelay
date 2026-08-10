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
const FLAG_KEYS: [(&str, bool); 14] = [
    ("ticket_sales_enabled", true),
    ("ticket_delivery_enabled", true),
    ("gate_redemption_enabled", true),
    ("mailer_enabled", true),
    ("communication_campaigns_enabled", false),
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
            ('communication_campaigns_enabled', false),
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

include!("ecosystem/control_plane.rs");
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

impl std::fmt::Display for EcosystemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest => f.write_str("bad request"),
            Self::NotFound => f.write_str("not found"),
            Self::Conflict => f.write_str("conflict"),
            Self::Unavailable => f.write_str("unavailable"),
            Self::Unexpected => f.write_str("unexpected"),
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
        assert_eq!(FLAG_KEYS.len(), 14);
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
