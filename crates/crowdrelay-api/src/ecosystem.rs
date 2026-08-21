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
use crowdrelay_application::{
    EcosystemControlPlaneRepository, EcosystemRepositoryError, RunReconciliationCommand,
    UpdateFeatureFlagCommand, UpdateShowChecklistCommand,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::FromRow;
use time::{Duration as TimeDuration, OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{sync::RwLock, time::timeout};
use uuid::Uuid;

use crate::{IDEMPOTENCY_KEY, Problem, request_id, ticket_qr::encode_ticket_qr};

const PRIVATE_NO_STORE: &str = "private, no-store";
pub(crate) const SHOW_SNAPSHOT_SCHEMA: u32 = 1;
const MAX_SHOW_PASSES: i64 = 10_000;
const MAX_LIST_LIMIT: i64 = 100;
const FLAG_CACHE_TTL: StdDuration = StdDuration::from_secs(1);
const MAX_FLAG_CACHE_ENTRIES: usize = 256;
const FLAG_KEYS: [(&str, bool); 16] = [
    ("ticket_sales_enabled", true),
    ("ticket_delivery_enabled", true),
    ("gate_redemption_enabled", true),
    ("mailer_enabled", true),
    ("communication_campaigns_enabled", false),
    ("push_delivery_enabled", false),
    ("meta_publish_enabled", true),
    ("bandsintown_sync_enabled", true),
    ("n8n_ingress_enabled", true),
    ("automatic_retry_enabled", true),
    ("draw_proofs_enabled", true),
    ("external_proof_anchoring_enabled", false),
    ("merch_inventory_enabled", false),
    ("reward_campaigns_enabled", false),
    ("merch_inventory_writes_enabled", false),
    ("area_legacy_imports_enabled", true),
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
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateFlagRequest {
    enabled: bool,
    reason: Option<String>,
    expected_version: Option<i64>,
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
    #[serde(with = "time::serde::rfc3339")]
    started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
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
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
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
    section: String,
    sort_order: i32,
    status: String,
    note: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
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
    #[serde(with = "time::serde::rfc3339")]
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
    bandsintown_sync: Option<BandsintownSyncStatus>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct BandsintownSyncStatus {
    #[serde(with = "time::serde::rfc3339::option")]
    last_synced_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    last_success_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    next_sync_at: OffsetDateTime,
    consecutive_failures: i32,
    last_error: Option<String>,
    in_progress: bool,
}

#[derive(Debug, Serialize)]
pub struct ManualEventSyncResult {
    provider: &'static str,
    queued: bool,
    already_running: bool,
}

#[derive(Debug, Serialize, FromRow)]
pub struct OverviewEvent {
    id: Uuid,
    slug: String,
    title: String,
    venue: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
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

pub async fn overview(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let future = async {
        ensure_default_flags(&state).await?;
        let (flags, last_reconciliation, open_findings, next_event, bandsintown_sync) = tokio::try_join!(
            load_flags(&state),
            load_last_reconciliation(&state),
            count_open_findings(&state),
            load_next_event(&state),
            load_bandsintown_sync(&state),
        )?;
        Ok::<_, EcosystemError>(EcosystemOverview {
            schema_version: SHOW_SNAPSHOT_SCHEMA,
            flags,
            last_reconciliation,
            open_findings,
            next_event,
            bandsintown_sync,
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
    if payload.trigger == "bandsintown_sync" {
        return respond(
            run(&state, request_bandsintown_sync(&state, &headers)).await,
            request_id_value,
        );
    }
    if payload.trigger != "manual" {
        return EcosystemError::BadRequest.into_response(request_id_value);
    }
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

pub(crate) async fn ensure_default_flags(state: &crate::AppState) -> Result<(), EcosystemError> {
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

async fn load_bandsintown_sync(
    state: &crate::AppState,
) -> Result<Option<BandsintownSyncStatus>, EcosystemError> {
    sqlx::query_as::<_, BandsintownSyncStatus>(
        r#"
        SELECT last_synced_at, last_success_at, next_sync_at, consecutive_failures, last_error,
               (sync_lease_until IS NOT NULL AND sync_lease_until > now()) AS in_progress
        FROM event_sources
        WHERE workspace_id = $1 AND provider = 'bandsintown' AND active
        ORDER BY id
        LIMIT 1
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(EcosystemError::sqlx)
}

async fn request_bandsintown_sync(
    state: &crate::AppState,
    headers: &HeaderMap,
) -> Result<ManualEventSyncResult, EcosystemError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let source = sqlx::query_as::<_, (Uuid, bool)>(
        r#"
        SELECT id, (sync_lease_until IS NOT NULL AND sync_lease_until > now()) AS in_progress
        FROM event_sources
        WHERE workspace_id = $1 AND provider = 'bandsintown' AND active
        ORDER BY id
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(EcosystemError::sqlx)?;
    let Some((source_id, already_running)) = source else {
        return Err(EcosystemError::NotFound);
    };
    let queued = if already_running {
        false
    } else {
        sqlx::query(
            "UPDATE event_sources SET next_sync_at=now() WHERE workspace_id=$1 AND id=$2 AND (sync_lease_until IS NULL OR sync_lease_until <= now())",
        )
        .bind(workspace_id)
        .bind(source_id)
        .execute(state.ticketing.pool())
        .await
        .map_err(EcosystemError::sqlx)?
        .rows_affected() == 1
    };
    if queued {
        sqlx::query(
            "INSERT INTO audit_events (workspace_id, actor_kind, action, target_type, target_id, request_id, metadata) VALUES ($1,'staff','event_source.sync_requested','event_source',$2,$3,$4)",
        )
        .bind(workspace_id)
        .bind(source_id.to_string())
        .bind(request_id(headers))
        .bind(serde_json::json!({"provider":"bandsintown"}))
        .execute(state.ticketing.pool())
        .await
        .map_err(EcosystemError::sqlx)?;
    }
    Ok(ManualEventSyncResult {
        provider: "bandsintown",
        queued,
        already_running,
    })
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

impl From<EcosystemRepositoryError> for EcosystemError {
    fn from(error: EcosystemRepositoryError) -> Self {
        match error {
            EcosystemRepositoryError::UnknownFlag | EcosystemRepositoryError::NotFound => {
                Self::NotFound
            }
            EcosystemRepositoryError::Conflict => Self::Conflict,
            EcosystemRepositoryError::Unexpected => Self::Unexpected,
        }
    }
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
    use super::flag_default;

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
mod tests;
