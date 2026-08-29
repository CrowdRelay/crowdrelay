//! Reach event repository — persistence for the unified reach ledger.
//!
//! This module provides the SQL functions to record, update, and query
//! reach events in `viryaos_reach_events`. The brain reads reach metrics
//! to learn which channels and templates produce the best reach-to-fan
//! conversion rates.
//!
//! See `crates/crowdrelay-brain/src/reach.rs` for the domain types.

use crowdrelay_brain::{ReachChannel, ReachEvent, ReachMetrics, ReachRecipientKind, ReachStatus};
use crowdrelay_domain::WorkspaceId;
use serde::Deserialize;
use sqlx::FromRow;
use time::OffsetDateTime;

use super::{PostgresAutopilotRepository, map_sqlx};
use crowdrelay_application::RepositoryError;

/// Records a new reach event. Returns the database-generated ID.
///
/// This is called by the channel-specific workers (community_executor,
/// push delivery, outreach execution) after they perform an outbound
/// contact attempt. The reach event is the brain's unified view of "who
/// did we contact, how, and what happened?"
#[allow(dead_code)]
pub(in crate::autopilot) async fn record_reach_event(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    event: &ReachEvent,
) -> Result<uuid::Uuid, RepositoryError> {
    let pool = &repo.pool;
    let id: (uuid::Uuid,) = sqlx::query_as(
        r#"
        INSERT INTO viryaos_reach_events
            (workspace_id, action_id, recipient_kind, recipient_id,
             channel, template_id, estimated_reach, status,
             sent_at, status_updated_at, converted_fan_id, converted_at,
             episode_id, metadata)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
        ON CONFLICT (action_id, recipient_id, channel) WHERE action_id IS NOT NULL
        DO UPDATE SET
            status = EXCLUDED.status,
            status_updated_at = EXCLUDED.status_updated_at,
            converted_fan_id = COALESCE(EXCLUDED.converted_fan_id, viryaos_reach_events.converted_fan_id),
            converted_at = COALESCE(EXCLUDED.converted_at, viryaos_reach_events.converted_at)
        RETURNING id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event.action_id)
    .bind(event.recipient_kind.as_str())
    .bind(&event.recipient_id)
    .bind(event.channel.as_str())
    .bind(&event.template_id)
    .bind(event.estimated_reach as i32)
    .bind(event.status.as_str())
    .bind(event.sent_at)
    .bind(event.status_updated_at)
    .bind(event.converted_fan_id)
    .bind(event.converted_at)
    .bind(&event.episode_id)
    .bind(&event.metadata)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(id.0)
}

/// Updates the status of a reach event. Used when a delivery confirmation,
/// reply, or conversion is received.
#[allow(dead_code)]
pub(in crate::autopilot) async fn update_reach_status(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    reach_event_id: uuid::Uuid,
    new_status: ReachStatus,
) -> Result<(), RepositoryError> {
    let pool = &repo.pool;
    let updated = sqlx::query(
        r#"
        UPDATE viryaos_reach_events
        SET status = $3, status_updated_at = now()
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(reach_event_id)
    .bind(new_status.as_str())
    .execute(pool)
    .await
    .map_err(map_sqlx)?;
    if updated.rows_affected() == 0 {
        return Err(RepositoryError::NotFound);
    }
    Ok(())
}

/// Marks a reach event as converted to a fan. This closes the
/// reach-to-fan attribution loop.
#[allow(dead_code)]
pub(in crate::autopilot) async fn convert_reach_to_fan(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    reach_event_id: uuid::Uuid,
    fan_id: uuid::Uuid,
) -> Result<(), RepositoryError> {
    let pool = &repo.pool;
    let updated = sqlx::query(
        r#"
        UPDATE viryaos_reach_events
        SET status = 'converted',
            converted_fan_id = $3,
            converted_at = now(),
            status_updated_at = now()
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(reach_event_id)
    .bind(fan_id)
    .execute(pool)
    .await
    .map_err(map_sqlx)?;
    if updated.rows_affected() == 0 {
        return Err(RepositoryError::NotFound);
    }
    Ok(())
}

/// Loads reach events for a workspace within a time window, optionally
/// filtered by channel or template.
#[allow(dead_code)]
pub(in crate::autopilot) async fn load_reach_events(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    since: OffsetDateTime,
    until: Option<OffsetDateTime>,
    channel_filter: Option<ReachChannel>,
    template_filter: Option<&str>,
) -> Result<Vec<ReachEvent>, RepositoryError> {
    let pool = &repo.pool;
    let rows: Vec<ReachEventRow> = sqlx::query_as(
        r#"
        SELECT
            workspace_id, action_id, recipient_kind, recipient_id,
            channel, template_id, estimated_reach, status,
            sent_at, status_updated_at, converted_fan_id, converted_at,
            episode_id, metadata
        FROM viryaos_reach_events
        WHERE workspace_id = $1
          AND sent_at >= $2
          AND ($3::timestamptz IS NULL OR sent_at < $3)
          AND ($4::text IS NULL OR channel = $4)
          AND ($5::text IS NULL OR template_id = $5)
        ORDER BY sent_at ASC
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(since)
    .bind(until)
    .bind(channel_filter.map(|c| c.as_str()))
    .bind(template_filter)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Loads aggregated reach metrics for a workspace within a time window.
///
/// This is the brain's primary read path for reach analytics. It returns
/// counts of each status type (sent, delivered, opened, clicked, replied,
/// converted, bounced, etc.) and the total estimated reach.
#[allow(dead_code)]
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

/// Finds reach events that could have converted a new fan. Used by the
/// fan conversion attribution system to link new fans to the reach events
/// that likely converted them.
///
/// This searches for reach events in the `delivered` or `clicked` status
/// that were sent before the fan's `created_at`, matching by metadata
/// (e.g. subreddit name) or timing.
#[allow(dead_code)]
pub(in crate::autopilot) async fn find_convertible_reach_events(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    fan_created_at: OffsetDateTime,
    subreddit_hint: Option<&str>,
) -> Result<Vec<(uuid::Uuid, ReachChannel, String)>, RepositoryError> {
    let pool = &repo.pool;
    let rows: Vec<(uuid::Uuid, String, String)> = sqlx::query_as(
        r#"
        SELECT id, channel, recipient_id
        FROM viryaos_reach_events
        WHERE workspace_id = $1
          AND status IN ('delivered', 'clicked', 'opened')
          AND sent_at < $2
          AND sent_at >= $2 - INTERVAL '14 days'
          AND ($3::text IS NULL OR metadata ->> 'subreddit' = $3)
        ORDER BY sent_at DESC
        LIMIT 10
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_created_at)
    .bind(subreddit_hint)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, channel_str, recipient_id)| {
            ReachChannel::parse(&channel_str).map(|ch| (id, ch, recipient_id))
        })
        .collect())
}

// ─── Internal types ──────────────────────────────────────────────────────

/// Database row for a reach event.
#[derive(Debug, FromRow, Deserialize)]
struct ReachEventRow {
    workspace_id: uuid::Uuid,
    action_id: Option<uuid::Uuid>,
    recipient_kind: String,
    recipient_id: String,
    channel: String,
    template_id: String,
    estimated_reach: i32,
    status: String,
    sent_at: OffsetDateTime,
    status_updated_at: OffsetDateTime,
    converted_fan_id: Option<uuid::Uuid>,
    converted_at: Option<OffsetDateTime>,
    episode_id: Option<String>,
    metadata: serde_json::Value,
}

impl From<ReachEventRow> for ReachEvent {
    fn from(row: ReachEventRow) -> Self {
        ReachEvent {
            workspace_id: row.workspace_id,
            action_id: row.action_id,
            recipient_kind: ReachRecipientKind::parse(&row.recipient_kind).unwrap_or_default(),
            recipient_id: row.recipient_id,
            channel: ReachChannel::parse(&row.channel).unwrap_or_default(),
            template_id: row.template_id,
            estimated_reach: row.estimated_reach.max(1) as u32,
            status: ReachStatus::parse(&row.status).unwrap_or_default(),
            sent_at: row.sent_at,
            status_updated_at: row.status_updated_at,
            converted_fan_id: row.converted_fan_id,
            converted_at: row.converted_at,
            episode_id: row.episode_id,
            metadata: row.metadata,
        }
    }
}

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
