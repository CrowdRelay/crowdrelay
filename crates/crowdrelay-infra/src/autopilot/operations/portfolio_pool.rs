//! The workspace's current candidate pool (5.1): what the eval ranked this
//! cycle, written whole so the roster's pooled read can re-rank every member
//! act's options under the organisation's own config.
//!
//! The table is current state, not a log — `replace` deletes the workspace's
//! rows and inserts the new pool in the same transaction, so a reader never
//! sees half of last cycle and half of this one, and a candidate that left
//! the pool leaves the read with it.

use sqlx::PgPool;

use super::*;

use crowdrelay_application::autopilot::PortfolioPoolEntry;

/// Replaces `workspace_id`'s pool rows with `entries`, atomically.
///
/// Serialized rather than UNNEST-batched: a pool is tens of rows and the
/// statement stays readable — the insert count is the candidate count, which
/// the eval already bounded when it scored them.
pub(in crate::autopilot) async fn replace_portfolio_pool(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    entries: &[PortfolioPoolEntry],
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    let mut transaction = pool.begin().await.map_err(map_sqlx)?;
    sqlx::query("DELETE FROM viryaos_portfolio_pool WHERE workspace_id = $1")
        .bind(workspace_id.into_uuid())
        .execute(&mut *transaction)
        .await
        .map_err(map_sqlx)?;
    for entry in entries {
        sqlx::query(
            r#"
            INSERT INTO viryaos_portfolio_pool (
                workspace_id, opportunity_key, opportunity_id, audience_key,
                source_context, action_key, decision_value, is_experimental,
                selected, rejection_reason, refreshed_at
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(&entry.opportunity_key)
        .bind(serde_json::to_value(&entry.opportunity_id).map_err(|_| RepositoryError::Unexpected)?)
        .bind(&entry.audience_key)
        .bind(&entry.source_context)
        .bind(&entry.action_key)
        .bind(serde_json::to_value(&entry.decision_value).map_err(|_| RepositoryError::Unexpected)?)
        .bind(entry.is_experimental)
        .bind(entry.selected)
        .bind(&entry.rejection_reason)
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(map_sqlx)?;
    }
    transaction.commit().await.map_err(map_sqlx)
}
