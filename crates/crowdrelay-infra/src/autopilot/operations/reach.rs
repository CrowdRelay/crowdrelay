//! Reach event repository — persistence for the unified reach ledger.
//!
//! This module provides the SQL functions to query reach events in
//! `viryaos_reach_events`. The brain reads reach metrics to learn which
//! channels and templates produce the best reach-to-fan conversion rates.
//!
//! Channel-specific workers (community_executor, push delivery, outreach
//! execution) record reach events via inline SQL at the call site. This
//! module provides the read-side query functions.
//!
//! See `crates/crowdrelay-brain/src/reach.rs` for the domain types.

use crowdrelay_brain::ReachMetrics;
use crowdrelay_domain::WorkspaceId;
use serde::Deserialize;
use sqlx::FromRow;
use time::OffsetDateTime;

use super::{PostgresAutopilotRepository, map_sqlx};
use crowdrelay_application::RepositoryError;

/// Loads aggregated reach metrics for a workspace within a time window.
///
/// This is the brain's primary read path for reach analytics. It returns
/// counts of each status type (sent, delivered, opened, clicked, replied,
/// converted, bounced, etc.) and the total estimated reach.
pub(in crate::autopilot) async fn load_reach_metrics(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    since: OffsetDateTime,
    until: Option<OffsetDateTime>,
) -> Result<ReachMetrics, RepositoryError> {
    let pool = &repo.pool;
    let row: ReachMetricsRow = sqlx::query_as(
        r#"
        SELECT
            COUNT(*)::bigint AS total_events,
            COALESCE(SUM(estimated_reach), 0)::bigint AS total_reach,
            COUNT(*) FILTER (WHERE status = 'delivered')::bigint AS delivered,
            COUNT(*) FILTER (WHERE status = 'opened')::bigint AS opened,
            COUNT(*) FILTER (WHERE status = 'clicked')::bigint AS clicked,
            COUNT(*) FILTER (WHERE status = 'replied')::bigint AS replied,
            COUNT(*) FILTER (WHERE status = 'positive_reply')::bigint AS positive_replies,
            COUNT(*) FILTER (WHERE status = 'declined')::bigint AS declined,
            COUNT(*) FILTER (WHERE status = 'converted')::bigint AS converted,
            COUNT(*) FILTER (WHERE status = 'bounced')::bigint AS bounced,
            COUNT(*) FILTER (WHERE status = 'complained')::bigint AS complained,
            COUNT(*) FILTER (WHERE status = 'failed')::bigint AS failed,
            COUNT(*) FILTER (WHERE status = 'ignored')::bigint AS ignored
        FROM viryaos_reach_events
        WHERE workspace_id = $1
          AND sent_at >= $2
          AND ($3::timestamptz IS NULL OR sent_at < $3)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(since)
    .bind(until)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(row.into())
}

// ─── Internal types ──────────────────────────────────────────────────────

/// Database row for aggregated reach metrics.
#[derive(Debug, FromRow, Deserialize)]
struct ReachMetricsRow {
    total_events: i64,
    total_reach: i64,
    delivered: i64,
    opened: i64,
    clicked: i64,
    replied: i64,
    positive_replies: i64,
    declined: i64,
    converted: i64,
    bounced: i64,
    complained: i64,
    failed: i64,
    ignored: i64,
}

impl From<ReachMetricsRow> for ReachMetrics {
    fn from(row: ReachMetricsRow) -> Self {
        ReachMetrics {
            total_events: row.total_events.max(0) as u64,
            total_reach: row.total_reach.max(0) as u64,
            delivered: row.delivered.max(0) as u64,
            opened: row.opened.max(0) as u64,
            clicked: row.clicked.max(0) as u64,
            replied: row.replied.max(0) as u64,
            positive_replies: row.positive_replies.max(0) as u64,
            declined: row.declined.max(0) as u64,
            converted: row.converted.max(0) as u64,
            bounced: row.bounced.max(0) as u64,
            complained: row.complained.max(0) as u64,
            failed: row.failed.max(0) as u64,
            ignored: row.ignored.max(0) as u64,
        }
    }
}
