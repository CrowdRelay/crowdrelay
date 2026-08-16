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
    push: QueueSummary,
    watchdog: WatchdogSummary,
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

#[derive(Debug, Default, Serialize)]
pub struct HttpRequestSummary {
    requests: u64,
    errors_4xx: u64,
    errors_5xx: u64,
    average_ms: u64,
    p50_ms: u64,
    p95_ms: u64,
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
    push_pending: i64,
    push_processing: i64,
    push_delivered_24h: i64,
    push_dead: i64,
    push_oldest_pending_seconds: i64,
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
    pub(crate) push_pending: i64,
    pub(crate) push_processing: i64,
    pub(crate) push_dead: i64,
    pub(crate) push_oldest_pending_seconds: i64,
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
    push_pending: i64,
    push_processing: i64,
    push_dead: i64,
    push_oldest_pending_seconds: i64,
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
    #[serde(with = "time::serde::rfc3339")]
    available_at: OffsetDateTime,
    last_error_kind: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    delivered_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
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
    #[serde(with = "time::serde::rfc3339")]
    available_at: OffsetDateTime,
    last_response_status: Option<i16>,
    last_error_kind: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    delivered_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    dead_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct DeliveryAttempt {
    attempt_number: i32,
    #[serde(with = "time::serde::rfc3339")]
    started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
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

#[derive(Debug, Serialize)]
pub struct ClearDeadDeliveriesResult {
    operation_id: Uuid,
    cleared: u64,
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
