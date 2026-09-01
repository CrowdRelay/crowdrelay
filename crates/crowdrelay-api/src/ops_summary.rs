//! Compact serialization types and first-party watchdog visibility for operations.

use serde::Serialize;
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Default, FromRow, Serialize)]
pub(crate) struct QueueSummary {
    pub(crate) pending: i64,
    pub(crate) processing: i64,
    pub(crate) delivered_24h: i64,
    pub(crate) dead: i64,
    pub(crate) cancelled: i64,
    pub(crate) oldest_pending_seconds: i64,
}

#[derive(Debug, Default, FromRow, Serialize)]
pub(crate) struct WatchdogSummary {
    active_alerts: i64,
    critical_alerts: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    last_observed_at: Option<OffsetDateTime>,
}

pub(crate) async fn load_watchdog_summary(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<WatchdogSummary, sqlx::Error> {
    sqlx::query_as::<_, WatchdogSummary>(
        r#"
        SELECT
            count(*) FILTER (WHERE active)::bigint AS active_alerts,
            count(*) FILTER (WHERE active AND severity = 'critical')::bigint AS critical_alerts,
            max(last_seen_at) AS last_observed_at
        FROM viryaos_ops_alert_state
        WHERE workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await
}

/// Whether the worker process is alive, and for how long it has not been.
///
/// The worker serves no HTTP, so nothing could ask it directly. It renews its
/// leadership lease every 15 seconds, which makes the lease age the one honest
/// heartbeat — and this summary is already on the operator's screen.
///
/// This exists because the worker was killed by a deploy and stayed dead for
/// over fifteen minutes while every dashboard showed green.
#[derive(Debug, Serialize)]
pub(crate) struct WorkerSummary {
    /// Seconds since the last lease renewal. Renewal is every 15s.
    pub(crate) lease_age_seconds: i64,
    /// False once the lease is stale enough that the process cannot be running.
    pub(crate) alive: bool,
}

/// Twice the renewal interval plus the lease term: unambiguous death, not a
/// slow cycle.
const WORKER_LEASE_DEAD_AFTER_SECONDS: i64 = 120;

pub(crate) async fn load_worker_summary(pool: &PgPool) -> Result<WorkerSummary, sqlx::Error> {
    // Not workspace-scoped: leadership is per deployment. A missing row means
    // no worker has ever run, which reads as dead rather than as healthy.
    let lease_age_seconds: i64 = sqlx::query_scalar(
        r#"
        SELECT COALESCE((
            SELECT EXTRACT(EPOCH FROM (
                now() - (expires_at - INTERVAL '60 seconds')
            ))::bigint
            FROM worker_leadership WHERE id = 1
        ), 999999)
        "#,
    )
    .fetch_one(pool)
    .await?;
    Ok(WorkerSummary {
        lease_age_seconds,
        alive: lease_age_seconds <= WORKER_LEASE_DEAD_AFTER_SECONDS,
    })
}
