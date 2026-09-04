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
    /// Liveness of the process that runs the brain, the outbox and metric sync.
    worker: WorkerSummary,
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
    push_suppressed: i64,
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
    pub(crate) push_suppressed: i64,
    pub(crate) push_oldest_pending_seconds: i64,
    /// Seconds since the worker last renewed its leadership lease.
    ///
    /// The worker renews every 15s; anything much above that means it is gone.
    /// A missing lease row reads as maximally stale, never as healthy.
    pub(crate) worker_lease_age_seconds: i64,
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
    push_suppressed: i64,
    push_oldest_pending_seconds: i64,
    worker_lease_age_seconds: i64,
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

/// One row of the native watchdog's alert state.
///
/// The counts in `OpsSummary.watchdog` say how many alerts are open; this says
/// which ones and why, so an operator does not have to open psql to find out.
#[derive(Debug, Serialize, FromRow)]
pub struct OpsAlert {
    alert_key: String,
    severity: String,
    summary: String,
    active: bool,
    #[serde(with = "time::serde::rfc3339")]
    first_seen_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    last_seen_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    last_alerted_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    recovered_at: Option<OffsetDateTime>,
    details: serde_json::Value,
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
pub struct PushDeliveryItem {
    id: Uuid,
    fan_id: Option<Uuid>,
    source_kind: String,
    title: String,
    status: String,
    attempt_count: i32,
    error_code: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    available_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    delivered_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    completed_at: Option<OffsetDateTime>,
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
    retention_loop: SignalRetentionLoop,
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

/// The retention loop end to end: a fan asks for a city, the city gets
/// coordinates, the fan becomes reachable, a show near them produces a
/// notification, and a push carries it to the device.
///
/// Each stage is counted separately because the loop has failed silently at
/// every one of them. Only the moderation queue was visible before, and that is
/// the one stage which does not block delivery at all.
#[derive(Debug, Serialize)]
pub struct SignalRetentionLoop {
    /// Requested cities with no latitude. These block delivery: no fan in one
    /// can be matched to a show, whatever their consent says.
    cities_awaiting_coordinates: i64,
    /// Requested cities that have coordinates, moderated or not.
    cities_resolved: i64,
    /// Fans whose chosen city has coordinates.
    fans_with_coordinates: i64,
    /// Fans the nearby-show loop would announce to today: opted in, active,
    /// located, and holding current marketing consent.
    nearby_eligible_fans: i64,
    /// Nearby-show notifications ever produced. `nearby_notifications_30d` on
    /// the activity summary is the recent window of the same thing.
    notifications_created: i64,
    /// Deliveries waiting for the push worker.
    pushes_queued: i64,
    /// Deliveries the push provider accepted.
    pushes_sent: i64,
    /// Deliveries the app acknowledged from the device.
    pushes_delivered: i64,
    /// Deliveries that stopped without reaching a device.
    pushes_failed: i64,
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
    cities_awaiting_coordinates: i64,
    cities_resolved: i64,
    fans_with_coordinates: i64,
    nearby_eligible_fans: i64,
    nearby_notifications_total: i64,
    pushes_queued: i64,
    pushes_sent: i64,
    pushes_delivered: i64,
    pushes_failed: i64,
}

#[derive(Debug, FromRow)]
struct ExistingAction {
    id: Uuid,
    action: String,
    target_type: String,
    target_id: Uuid,
}
