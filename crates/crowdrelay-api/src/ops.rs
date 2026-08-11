//! Administrative operations visibility and audited recovery actions.
//!
//! The control plane intentionally exposes metadata only: event payloads,
//! signing material, endpoint URLs, and fan data never leave this module.

use std::{future::Future, time::Duration};

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use crowdrelay_domain::WorkspaceId;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use tokio::time::timeout;
use uuid::Uuid;

use crate::{IDEMPOTENCY_KEY, Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";
const DEFAULT_PAGE_SIZE: i64 = 50;
const MAX_PAGE_SIZE: i64 = 100;

/// Database context for the administrative operations control plane.
#[derive(Clone)]
pub struct OpsState {
    workspace_id: WorkspaceId,
    pool: PgPool,
    operation_timeout: Duration,
}

impl OpsState {
    /// Creates operations state scoped to one CrowdRelay workspace.
    #[must_use]
    pub fn new(workspace_id: WorkspaceId, pool: PgPool, operation_timeout: Duration) -> Self {
        Self {
            workspace_id,
            pool,
            operation_timeout,
        }
    }

    pub(crate) async fn metrics_snapshot(&self) -> Result<OpsMetricsSnapshot, OpsError> {
        run_with_timeout(self.operation_timeout, load_metrics_snapshot(self)).await
    }

    #[must_use]
    pub(crate) const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }
}

#[derive(Debug, Serialize)]
pub struct OpsSummary {
    outbox: QueueSummary,
    deliveries: QueueSummary,
    http: HttpRequestSummary,
    database: DatabaseRuntimeSummary,
    area: AreaRuntimeSummary,
    schema_version: u32,
    release: String,
}

#[derive(Debug, Default, Serialize)]
pub struct AreaRuntimeSummary {
    credits_total: i64,
    vouchers_issued: i64,
    stale_voucher_reservations: i64,
    ticket_rewards_issued: i64,
    stale_ticket_reward_reservations: i64,
    legacy_imported_players: i64,
}

#[derive(Debug, Default, FromRow)]
struct AreaRuntimeRow {
    credits_total: i64,
    vouchers_issued: i64,
    stale_voucher_reservations: i64,
    ticket_rewards_issued: i64,
    stale_ticket_reward_reservations: i64,
    legacy_imported_players: i64,
}

#[derive(Debug, Serialize)]
pub struct DatabaseRuntimeSummary {
    server_version_num: i32,
    io_method: Option<String>,
    io_workers: Option<i32>,
    io_max_concurrency: Option<i32>,
    effective_io_concurrency: Option<i32>,
    maintenance_io_concurrency: Option<i32>,
    io_combine_limit_bytes: Option<i64>,
    io_max_combine_limit_bytes: Option<i64>,
    async_io_active: bool,
}

#[derive(Debug, FromRow)]
struct DatabaseRuntimeRow {
    server_version_num: i32,
    io_method: Option<String>,
    io_workers: Option<i32>,
    io_max_concurrency: Option<i32>,
    effective_io_concurrency: Option<i32>,
    maintenance_io_concurrency: Option<i32>,
    io_combine_limit_bytes: Option<i64>,
    io_max_combine_limit_bytes: Option<i64>,
}

#[derive(Debug, Default, Serialize)]
pub struct HttpRequestSummary {
    requests: u64,
    errors_4xx: u64,
    errors_5xx: u64,
    average_ms: u64,
    p50_ms: u64,
    p95_ms: u64,
}

#[derive(Debug, Serialize, FromRow)]
pub struct QueueSummary {
    pending: i64,
    processing: i64,
    delivered_24h: i64,
    dead: i64,
    cancelled: i64,
    oldest_pending_seconds: i64,
}

#[derive(Debug, FromRow)]
struct OpsSummaryRow {
    outbox_pending: i64,
    outbox_processing: i64,
    outbox_delivered_24h: i64,
    outbox_dead: i64,
    outbox_oldest_pending_seconds: i64,
    delivery_pending: i64,
    delivery_processing: i64,
    delivery_delivered_24h: i64,
    delivery_dead: i64,
    delivery_cancelled: i64,
    delivery_oldest_pending_seconds: i64,
}

#[derive(Debug, Clone, Copy, Default, FromRow)]
pub(crate) struct OpsMetricsSnapshot {
    pub(crate) outbox_pending: i64,
    pub(crate) outbox_processing: i64,
    pub(crate) outbox_dead: i64,
    pub(crate) outbox_oldest_pending_seconds: i64,
    pub(crate) delivery_pending: i64,
    pub(crate) delivery_processing: i64,
    pub(crate) delivery_dead: i64,
    pub(crate) delivery_cancelled: i64,
    pub(crate) delivery_oldest_pending_seconds: i64,
}

#[derive(Debug, FromRow)]
struct OpsMetricsRow {
    outbox_pending: i64,
    outbox_processing: i64,
    outbox_dead: i64,
    outbox_oldest_pending_seconds: i64,
    delivery_pending: i64,
    delivery_processing: i64,
    delivery_dead: i64,
    delivery_cancelled: i64,
    delivery_oldest_pending_seconds: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    status: Option<QueueStatus>,
    limit: Option<i64>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QueueStatus {
    Pending,
    Processing,
    Delivered,
    Dead,
    Cancelled,
}

impl QueueStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Processing => "processing",
            Self::Delivered => "delivered",
            Self::Dead => "dead",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Serialize, FromRow)]
pub struct OutboxItem {
    id: Uuid,
    event_type: String,
    event_version: i32,
    status: String,
    attempts: i32,
    max_attempts: i32,
    available_at: OffsetDateTime,
    last_error_kind: Option<String>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    delivered_at: Option<OffsetDateTime>,
    dead_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct DeliveryItem {
    id: Uuid,
    outbox_event_id: Uuid,
    event_type: String,
    endpoint_name: String,
    endpoint_active: bool,
    status: String,
    attempt_count: i32,
    max_attempts: i32,
    available_at: OffsetDateTime,
    last_response_status: Option<i16>,
    last_error_kind: Option<String>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    delivered_at: Option<OffsetDateTime>,
    dead_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct DeliveryAttempt {
    attempt_number: i32,
    started_at: OffsetDateTime,
    finished_at: OffsetDateTime,
    outcome: String,
    response_status: Option<i16>,
    error_kind: Option<String>,
    duration_ms: i32,
}

#[derive(Debug, Serialize)]
pub struct DeliveryDetails {
    delivery: DeliveryItem,
    attempts: Vec<DeliveryAttempt>,
}

#[derive(Debug, Serialize)]
pub struct RetryResult {
    operation_id: Uuid,
    target_type: &'static str,
    target_id: Uuid,
    status: &'static str,
    replayed: bool,
}

/// Aggregate-only owner view of Virya Signal health and growth.
///
/// The response intentionally contains no e-mail addresses, display names,
/// fan identifiers, consent history, or raw event payloads.
#[derive(Debug, Serialize)]
pub struct SignalOverview {
    #[serde(with = "time::serde::rfc3339")]
    generated_at: OffsetDateTime,
    summary: SignalFanSummary,
    activity: SignalActivitySummary,
    top_cities: Vec<SignalCitySummary>,
    unavailable_sources: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct SignalFanSummary {
    total_fans: i64,
    active_fans: i64,
    pending_fans: i64,
    unsubscribed_fans: i64,
    suppressed_fans: i64,
    marketing_opted_in: i64,
    nearby_enabled: i64,
}

#[derive(Debug, Serialize)]
pub struct SignalActivitySummary {
    new_fans_7d: i64,
    new_fans_30d: i64,
    referral_attributions_total: i64,
    referral_attributions_30d: i64,
    event_interests_total: i64,
    event_interests_30d: i64,
    nearby_notifications_30d: i64,
    pending_city_requests: i64,
}

#[derive(Debug, Serialize, FromRow)]
pub struct SignalCitySummary {
    slug: String,
    name: String,
    country_code: String,
    active_fans: i64,
}

#[derive(Debug, FromRow)]
struct SignalSummaryRow {
    total_fans: i64,
    active_fans: i64,
    pending_fans: i64,
    unsubscribed_fans: i64,
    suppressed_fans: i64,
    marketing_opted_in: i64,
    nearby_enabled: i64,
    new_fans_7d: i64,
    new_fans_30d: i64,
    referral_attributions_total: i64,
    referral_attributions_30d: i64,
    event_interests_total: i64,
    event_interests_30d: i64,
    nearby_notifications_30d: i64,
    pending_city_requests: i64,
}

#[derive(Debug, FromRow)]
struct ExistingAction {
    id: Uuid,
    action: String,
    target_type: String,
    target_id: Uuid,
}
include!("ops_timeline.rs");
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

async fn load_signal_summary(state: &OpsState) -> Result<SignalSummaryRow, OpsError> {
    sqlx::query_as::<_, SignalSummaryRow>(
        r#"
        WITH latest_marketing AS (
            SELECT DISTINCT ON (fan_id)
                   fan_id, granted
            FROM fan_consents
            WHERE workspace_id = $1
              AND purpose = 'marketing'
            ORDER BY fan_id, recorded_at DESC, id DESC
        ),
        fan_summary AS (
            SELECT
                count(*) AS total_fans,
                count(*) FILTER (WHERE status = 'active') AS active_fans,
                count(*) FILTER (WHERE status = 'pending') AS pending_fans,
                count(*) FILTER (WHERE status = 'unsubscribed') AS unsubscribed_fans,
                count(*) FILTER (WHERE status = 'suppressed') AS suppressed_fans,
                count(*) FILTER (
                    WHERE status = 'active'
                      AND created_at >= now() - interval '7 days'
                ) AS new_fans_7d,
                count(*) FILTER (
                    WHERE status = 'active'
                      AND created_at >= now() - interval '30 days'
                ) AS new_fans_30d
            FROM fans
            WHERE workspace_id = $1
        ),
        consent_summary AS (
            SELECT count(*) FILTER (
                WHERE consent.granted
                  AND fan.status = 'active'
            ) AS marketing_opted_in
            FROM latest_marketing AS consent
            JOIN fans AS fan
              ON fan.workspace_id = $1
             AND fan.id = consent.fan_id
        ),
        location_summary AS (
            SELECT
                count(*) FILTER (
                    WHERE preference.nearby_gigs_enabled
                      AND fan.status = 'active'
                ) AS nearby_enabled,
                count(DISTINCT preference.city_id) FILTER (
                    WHERE city.moderation_status = 'pending'
                      AND fan.status = 'active'
                ) AS pending_city_requests
            FROM fan_location_preferences AS preference
            JOIN fans AS fan
              ON fan.workspace_id = preference.workspace_id
             AND fan.id = preference.fan_id
            JOIN cities AS city
              ON city.id = preference.city_id
            WHERE preference.workspace_id = $1
        ),
        referral_summary AS (
            SELECT
                count(*) AS referral_attributions_total,
                count(*) FILTER (
                    WHERE accepted_at >= now() - interval '30 days'
                ) AS referral_attributions_30d
            FROM referral_attributions
            WHERE workspace_id = $1
        ),
        interest_summary AS (
            SELECT
                count(*) AS event_interests_total,
                count(*) FILTER (
                    WHERE created_at >= now() - interval '30 days'
                ) AS event_interests_30d
            FROM event_interests
            WHERE workspace_id = $1
        ),
        notification_summary AS (
            SELECT count(*) FILTER (
                WHERE created_at >= now() - interval '30 days'
            ) AS nearby_notifications_30d
            FROM nearby_gig_notifications
            WHERE workspace_id = $1
        )
        SELECT
            fan_summary.total_fans,
            fan_summary.active_fans,
            fan_summary.pending_fans,
            fan_summary.unsubscribed_fans,
            fan_summary.suppressed_fans,
            consent_summary.marketing_opted_in,
            location_summary.nearby_enabled,
            fan_summary.new_fans_7d,
            fan_summary.new_fans_30d,
            referral_summary.referral_attributions_total,
            referral_summary.referral_attributions_30d,
            interest_summary.event_interests_total,
            interest_summary.event_interests_30d,
            notification_summary.nearby_notifications_30d,
            location_summary.pending_city_requests
        FROM fan_summary
        CROSS JOIN consent_summary
        CROSS JOIN location_summary
        CROSS JOIN referral_summary
        CROSS JOIN interest_summary
        CROSS JOIN notification_summary
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_one(&state.pool)
    .await
    .map_err(OpsError::sqlx)
}

async fn load_signal_top_cities(state: &OpsState) -> Result<Vec<SignalCitySummary>, OpsError> {
    sqlx::query_as::<_, SignalCitySummary>(
        r#"
        SELECT
            city.slug,
            city.name,
            city.country_code::text AS country_code,
            count(DISTINCT fan.id) AS active_fans
        FROM fan_city_interests AS interest
        JOIN fans AS fan
          ON fan.workspace_id = interest.workspace_id
         AND fan.id = interest.fan_id
        JOIN cities AS city
          ON city.id = interest.city_id
        WHERE interest.workspace_id = $1
          AND fan.status = 'active'
        GROUP BY city.slug, city.name, city.country_code
        ORDER BY active_fans DESC, city.name ASC, city.slug ASC
        LIMIT 10
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_all(&state.pool)
    .await
    .map_err(OpsError::sqlx)
}

async fn load_summary(state: &OpsState) -> Result<OpsSummary, OpsError> {
    let row = sqlx::query_as::<_, OpsSummaryRow>(
        r#"
        WITH outbox AS (
            SELECT
                count(*) FILTER (WHERE status = 'pending')::bigint AS pending,
                count(*) FILTER (WHERE status = 'processing')::bigint AS processing,
                count(*) FILTER (
                    WHERE status = 'delivered'
                      AND delivered_at >= now() - interval '24 hours'
                )::bigint AS delivered_24h,
                count(*) FILTER (WHERE status = 'dead')::bigint AS dead,
                COALESCE(EXTRACT(EPOCH FROM (now() - min(available_at) FILTER (
                    WHERE status = 'pending' AND available_at <= now()
                )))::bigint, 0) AS oldest_pending_seconds
            FROM outbox_events
            WHERE workspace_id = $1
        ),
        deliveries AS (
            SELECT
                count(*) FILTER (WHERE status = 'pending')::bigint AS pending,
                count(*) FILTER (WHERE status = 'processing')::bigint AS processing,
                count(*) FILTER (
                    WHERE status = 'delivered'
                      AND delivered_at >= now() - interval '24 hours'
                )::bigint AS delivered_24h,
                count(*) FILTER (WHERE status = 'dead')::bigint AS dead,
                count(*) FILTER (WHERE status = 'cancelled')::bigint AS cancelled,
                COALESCE(EXTRACT(EPOCH FROM (now() - min(available_at) FILTER (
                    WHERE status = 'pending' AND available_at <= now()
                )))::bigint, 0) AS oldest_pending_seconds
            FROM webhook_deliveries
            WHERE workspace_id = $1
        )
        SELECT
            outbox.pending AS outbox_pending,
            outbox.processing AS outbox_processing,
            outbox.delivered_24h AS outbox_delivered_24h,
            outbox.dead AS outbox_dead,
            outbox.oldest_pending_seconds AS outbox_oldest_pending_seconds,
            deliveries.pending AS delivery_pending,
            deliveries.processing AS delivery_processing,
            deliveries.delivered_24h AS delivery_delivered_24h,
            deliveries.dead AS delivery_dead,
            deliveries.cancelled AS delivery_cancelled,
            deliveries.oldest_pending_seconds AS delivery_oldest_pending_seconds
        FROM outbox CROSS JOIN deliveries
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_one(&state.pool)
    .await
    .map_err(OpsError::sqlx)?;

    let database = sqlx::query_as::<_, DatabaseRuntimeRow>(
        r#"
        SELECT
            current_setting('server_version_num')::integer AS server_version_num,
            current_setting('io_method', true) AS io_method,
            NULLIF(current_setting('io_workers', true), '')::integer AS io_workers,
            NULLIF(current_setting('io_max_concurrency', true), '')::integer AS io_max_concurrency,
            NULLIF(current_setting('effective_io_concurrency', true), '')::integer
                AS effective_io_concurrency,
            NULLIF(current_setting('maintenance_io_concurrency', true), '')::integer
                AS maintenance_io_concurrency,
            CASE
                WHEN NULLIF(current_setting('io_combine_limit', true), '') IS NULL THEN NULL
                ELSE pg_size_bytes(current_setting('io_combine_limit', true))::bigint
            END AS io_combine_limit_bytes,
            CASE
                WHEN NULLIF(current_setting('io_max_combine_limit', true), '') IS NULL THEN NULL
                ELSE pg_size_bytes(current_setting('io_max_combine_limit', true))::bigint
            END AS io_max_combine_limit_bytes
        "#,
    )
    .fetch_one(&state.pool)
    .await
    .map_err(OpsError::sqlx)?;
    let area = sqlx::query_as::<_, AreaRuntimeRow>(
        r#"
        SELECT
            COALESCE((SELECT sum(delta)::bigint FROM area_credit_ledger WHERE workspace_id = $1), 0) AS credits_total,
            COALESCE((SELECT count(*)::bigint FROM area_reward_vouchers WHERE workspace_id = $1 AND status = 'issued'), 0) AS vouchers_issued,
            COALESCE((SELECT count(*)::bigint FROM area_reward_vouchers WHERE workspace_id = $1 AND status = 'reserved' AND reserved_until < now()), 0) AS stale_voucher_reservations,
            COALESCE((SELECT count(*)::bigint FROM area_ticket_rewards WHERE workspace_id = $1 AND status = 'issued'), 0) AS ticket_rewards_issued,
            COALESCE((SELECT count(*)::bigint FROM area_ticket_rewards WHERE workspace_id = $1 AND status = 'reserved' AND reservation_expires_at < now()), 0) AS stale_ticket_reward_reservations,
            COALESCE((SELECT count(*)::bigint FROM area_legacy_wallet_imports WHERE workspace_id = $1), 0) AS legacy_imported_players
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_one(&state.pool)
    .await
    .map_err(OpsError::sqlx)?;

    let database = DatabaseRuntimeSummary {
        server_version_num: database.server_version_num,
        async_io_active: database.server_version_num >= 180_000
            && database.effective_io_concurrency.unwrap_or_default() > 0
            && database.io_method.as_deref() != Some("sync"),
        io_method: database.io_method,
        io_workers: database.io_workers,
        io_max_concurrency: database.io_max_concurrency,
        effective_io_concurrency: database.effective_io_concurrency,
        maintenance_io_concurrency: database.maintenance_io_concurrency,
        io_combine_limit_bytes: database.io_combine_limit_bytes,
        io_max_combine_limit_bytes: database.io_max_combine_limit_bytes,
    };

    Ok(OpsSummary {
        outbox: QueueSummary {
            pending: row.outbox_pending,
            processing: row.outbox_processing,
            delivered_24h: row.outbox_delivered_24h,
            dead: row.outbox_dead,
            cancelled: 0,
            oldest_pending_seconds: row.outbox_oldest_pending_seconds,
        },
        deliveries: QueueSummary {
            pending: row.delivery_pending,
            processing: row.delivery_processing,
            delivered_24h: row.delivery_delivered_24h,
            dead: row.delivery_dead,
            cancelled: row.delivery_cancelled,
            oldest_pending_seconds: row.delivery_oldest_pending_seconds,
        },
        http: http_request_summary(crate::http_metrics().snapshot()),
        database,
        area: AreaRuntimeSummary {
            credits_total: area.credits_total,
            vouchers_issued: area.vouchers_issued,
            stale_voucher_reservations: area.stale_voucher_reservations,
            ticket_rewards_issued: area.ticket_rewards_issued,
            stale_ticket_reward_reservations: area.stale_ticket_reward_reservations,
            legacy_imported_players: area.legacy_imported_players,
        },
        schema_version: 40,
        release: option_env!("CROWDRELAY_RELEASE")
            .unwrap_or(env!("CARGO_PKG_VERSION"))
            .to_owned(),
    })
}

fn http_request_summary(snapshot: crate::http_metrics::HttpMetricsSnapshot) -> HttpRequestSummary {
    let average_ms = snapshot
        .latency_micros_sum
        .checked_div(snapshot.total)
        .unwrap_or_default()
        / 1_000;
    HttpRequestSummary {
        requests: snapshot.total,
        errors_4xx: snapshot.errors_4xx,
        errors_5xx: snapshot.errors_5xx,
        average_ms,
        p50_ms: percentile_bucket_ms(snapshot, 50),
        p95_ms: percentile_bucket_ms(snapshot, 95),
    }
}

fn percentile_bucket_ms(
    snapshot: crate::http_metrics::HttpMetricsSnapshot,
    percentile: u64,
) -> u64 {
    if snapshot.total == 0 {
        return 0;
    }
    let target = snapshot.total.saturating_mul(percentile).div_ceil(100);
    for (bound, count) in [
        (50, snapshot.le_50_ms),
        (100, snapshot.le_100_ms),
        (250, snapshot.le_250_ms),
        (500, snapshot.le_500_ms),
        (1_000, snapshot.le_1000_ms),
        (2_500, snapshot.le_2500_ms),
        (5_000, snapshot.le_5000_ms),
    ] {
        if count >= target {
            return bound;
        }
    }
    5_001
}

async fn load_metrics_snapshot(state: &OpsState) -> Result<OpsMetricsSnapshot, OpsError> {
    // Prometheus scrapes do not need the 24-hour delivered counters used by the
    // admin summary. Keep this query narrow to reduce CPU and buffer churn.
    let row = sqlx::query_as::<_, OpsMetricsRow>(
        r#"
        WITH outbox AS (
            SELECT
                count(*) FILTER (WHERE status = 'pending')::bigint AS pending,
                count(*) FILTER (WHERE status = 'processing')::bigint AS processing,
                count(*) FILTER (WHERE status = 'dead')::bigint AS dead,
                COALESCE(EXTRACT(EPOCH FROM (now() - min(available_at) FILTER (
                    WHERE status = 'pending' AND available_at <= now()
                )))::bigint, 0) AS oldest_pending_seconds
            FROM outbox_events
            WHERE workspace_id = $1
        ),
        deliveries AS (
            SELECT
                count(*) FILTER (WHERE status = 'pending')::bigint AS pending,
                count(*) FILTER (WHERE status = 'processing')::bigint AS processing,
                count(*) FILTER (WHERE status = 'dead')::bigint AS dead,
                count(*) FILTER (WHERE status = 'cancelled')::bigint AS cancelled,
                COALESCE(EXTRACT(EPOCH FROM (now() - min(available_at) FILTER (
                    WHERE status = 'pending' AND available_at <= now()
                )))::bigint, 0) AS oldest_pending_seconds
            FROM webhook_deliveries
            WHERE workspace_id = $1
        )
        SELECT
            outbox.pending AS outbox_pending,
            outbox.processing AS outbox_processing,
            outbox.dead AS outbox_dead,
            outbox.oldest_pending_seconds AS outbox_oldest_pending_seconds,
            deliveries.pending AS delivery_pending,
            deliveries.processing AS delivery_processing,
            deliveries.dead AS delivery_dead,
            deliveries.cancelled AS delivery_cancelled,
            deliveries.oldest_pending_seconds AS delivery_oldest_pending_seconds
        FROM outbox CROSS JOIN deliveries
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_one(&state.pool)
    .await
    .map_err(OpsError::sqlx)?;

    Ok(OpsMetricsSnapshot {
        outbox_pending: row.outbox_pending,
        outbox_processing: row.outbox_processing,
        outbox_dead: row.outbox_dead,
        outbox_oldest_pending_seconds: row.outbox_oldest_pending_seconds,
        delivery_pending: row.delivery_pending,
        delivery_processing: row.delivery_processing,
        delivery_dead: row.delivery_dead,
        delivery_cancelled: row.delivery_cancelled,
        delivery_oldest_pending_seconds: row.delivery_oldest_pending_seconds,
    })
}

async fn load_delivery(state: &OpsState, id: Uuid) -> Result<Option<DeliveryDetails>, OpsError> {
    let delivery = sqlx::query_as::<_, DeliveryItem>(
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
        WHERE delivery.workspace_id = $1 AND delivery.id = $2
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(OpsError::sqlx)?;
    let Some(delivery) = delivery else {
        return Ok(None);
    };
    let attempts = sqlx::query_as::<_, DeliveryAttempt>(
        r#"
        SELECT attempt_number, started_at, finished_at, outcome,
               response_status, error_kind, duration_ms
        FROM webhook_delivery_attempts
        WHERE workspace_id = $1 AND delivery_id = $2
        ORDER BY attempt_number DESC
        LIMIT 100
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(id)
    .fetch_all(&state.pool)
    .await
    .map_err(OpsError::sqlx)?;
    Ok(Some(DeliveryDetails { delivery, attempts }))
}

fn page_size(limit: Option<i64>) -> Result<i64, OpsError> {
    let limit = limit.unwrap_or(DEFAULT_PAGE_SIZE);
    (1..=MAX_PAGE_SIZE)
        .contains(&limit)
        .then_some(limit)
        .ok_or(OpsError::BadRequest)
}

fn parse_id(id: &str) -> Result<Uuid, OpsError> {
    Uuid::parse_str(id).map_err(|_| OpsError::BadRequest)
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, OpsError> {
    let value = headers
        .get(&IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| {
            (8..=128).contains(&value.len())
                && value.bytes().all(|byte| (b'!'..=b'~').contains(&byte))
        })
        .ok_or(OpsError::BadRequest)?;
    Ok(value.to_owned())
}

async fn run_with_timeout<T, E>(
    duration: Duration,
    future: impl Future<Output = Result<T, E>>,
) -> Result<T, OpsError>
where
    E: Into<OpsError>,
{
    timeout(duration, future)
        .await
        .map_err(|_| OpsError::Unavailable)?
        .map_err(Into::into)
}

fn private_json<T: Serialize>(status: StatusCode, body: T) -> Response {
    (status, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(body)).into_response()
}

#[derive(Debug)]
pub(crate) enum OpsError {
    BadRequest,
    NotFound,
    Conflict,
    InactiveEndpoint,
    Unavailable,
    Unexpected,
}

impl OpsError {
    fn sqlx(error: sqlx::Error) -> Self {
        tracing::error!(error = %error, "operations database query failed");
        Self::Unexpected
    }

    fn into_response(self, request_id: Option<String>) -> Response {
        match self {
            Self::BadRequest => Problem::bad_request(request_id).private().into_response(),
            Self::NotFound => Problem::not_found(request_id).private().into_response(),
            Self::Conflict | Self::InactiveEndpoint => {
                Problem::conflict(request_id).private().into_response()
            }
            Self::Unavailable => Problem::service_unavailable(request_id)
                .private()
                .into_response(),
            Self::Unexpected => Problem::internal(request_id).private().into_response(),
        }
    }
}

impl From<sqlx::Error> for OpsError {
    fn from(error: sqlx::Error) -> Self {
        Self::sqlx(error)
    }
}

#[cfg(test)]
mod signal_tests {
    use super::{SignalCitySummary, SignalSummaryRow, signal_overview_from_row};

    #[test]
    fn signal_overview_payload_is_aggregate_only() {
        let overview = signal_overview_from_row(
            SignalSummaryRow {
                total_fans: 14,
                active_fans: 10,
                pending_fans: 2,
                unsubscribed_fans: 1,
                suppressed_fans: 1,
                marketing_opted_in: 9,
                nearby_enabled: 8,
                new_fans_7d: 3,
                new_fans_30d: 6,
                referral_attributions_total: 11,
                referral_attributions_30d: 4,
                event_interests_total: 20,
                event_interests_30d: 7,
                nearby_notifications_30d: 5,
                pending_city_requests: 1,
            },
            vec![SignalCitySummary {
                slug: "wroclaw".to_owned(),
                name: "Wrocław".to_owned(),
                country_code: "PL".to_owned(),
                active_fans: 8,
            }],
            Vec::new(),
        );

        let json = match serde_json::to_string(&overview) {
            Ok(json) => json,
            Err(error) => panic!("signal overview serialization failed: {error}"),
        };
        assert!(json.contains("\"active_fans\":10"));
        assert!(json.contains("\"top_cities\""));
        assert!(!json.contains("email"));
        assert!(!json.contains("display_name"));
        assert!(!json.contains("fan_id"));
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::{OpsError, idempotency_key, page_size, parse_id};

    #[test]
    fn pagination_is_bounded() {
        assert_eq!(page_size(None).ok(), Some(50));
        assert_eq!(page_size(Some(1)).ok(), Some(1));
        assert_eq!(page_size(Some(100)).ok(), Some(100));
        assert!(matches!(page_size(Some(0)), Err(OpsError::BadRequest)));
        assert!(matches!(page_size(Some(101)), Err(OpsError::BadRequest)));
    }

    #[test]
    fn retry_identifiers_and_keys_are_strict() {
        assert!(parse_id("0198f120-f478-7d55-b1b8-5f3a4118dc75").is_ok());
        assert!(matches!(parse_id("not-a-uuid"), Err(OpsError::BadRequest)));

        let mut headers = HeaderMap::new();
        assert!(matches!(
            idempotency_key(&headers),
            Err(OpsError::BadRequest)
        ));
        headers.insert(
            "idempotency-key",
            HeaderValue::from_static("ops-retry-0198f120"),
        );
        assert_eq!(
            idempotency_key(&headers).ok().as_deref(),
            Some("ops-retry-0198f120")
        );
        headers.insert(
            "idempotency-key",
            HeaderValue::from_static("retry with spaces"),
        );
        assert!(matches!(
            idempotency_key(&headers),
            Err(OpsError::BadRequest)
        ));
    }
}
