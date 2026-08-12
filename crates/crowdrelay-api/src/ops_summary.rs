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
