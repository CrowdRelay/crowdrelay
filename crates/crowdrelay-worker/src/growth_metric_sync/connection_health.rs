//! Recording whether a fanbase connection is actually syncing.
//!
//! Separate from the metric sync itself because it answers a different
//! question. `fanbase_connections.status` says whether credentials are
//! present; these two writes say whether the channel works. For five of
//! production's connections — discord, telegram, lastfm, facebook, instagram
//! — those answers had been opposite since the day they were created, and
//! only the log knew.

use super::{DueConnection, GrowthMetricSyncError, GrowthMetricSyncWorker};
use tokio::time::error::Elapsed;

impl GrowthMetricSyncWorker {
    /// Records one connection's sync outcome and logs it.
    ///
    /// Takes the whole result so the three cases stay together: a success
    /// clears any recorded failure, an error and a timeout each keep their own
    /// message. Splitting them across the caller is how the log and the row
    /// drift apart.
    pub(super) async fn record_sync_outcome(
        &self,
        conn: &DueConnection,
        result: &Result<Result<(), GrowthMetricSyncError>, Elapsed>,
    ) {
        match result {
            Ok(Ok(())) => {
                if let Err(error) = self.record_sync_success(conn).await {
                    tracing::warn!(
                        connection_id = %conn.id,
                        error = %error,
                        "could not record a successful sync on the connection"
                    );
                }
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    connection_id = %conn.id,
                    platform = %conn.platform,
                    error = %error,
                    "growth metric sync failed for connection"
                );
                self.record_sync_failure(conn, &error.to_string()).await;
            }
            Err(_) => {
                tracing::warn!(
                    connection_id = %conn.id,
                    platform = %conn.platform,
                    "growth metric sync timed out for connection (20s)"
                );
                self.record_sync_failure(
                    conn,
                    "sync timed out after 20s with no response from the provider",
                )
                .await;
            }
        }
    }

    /// Marks a connection as syncing cleanly, clearing any recorded failure.
    /// A connection that was `'unverified'` (creation-time probe could not
    /// confirm the identity) is promoted to `'connected'` — the successful
    /// sync is the proof that the credential works.
    pub(super) async fn record_sync_success(
        &self,
        conn: &DueConnection,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE fanbase_connections
            SET last_sync_at = now(),
                last_sync_error = NULL,
                last_sync_failed_at = NULL,
                status = CASE WHEN status = 'unverified' THEN 'connected' ELSE status END,
                updated_at = now()
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(conn.workspace_id)
        .bind(conn.id)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    /// Records why a connection's sync failed.
    ///
    /// Never propagates: this runs inside the per-connection arm of the sweep,
    /// and a bookkeeping write that fails must not stop the remaining
    /// connections from syncing. It logs instead, which is strictly better
    /// than the silence this replaces.
    ///
    /// `status` is left alone on purpose — it means "credentials are
    /// present", and folding a provider outage into it would make a transient
    /// failure look like a revoked credential.
    pub(super) async fn record_sync_failure(&self, conn: &DueConnection, error: &str) {
        let trimmed: String = error.chars().take(500).collect();
        let written = sqlx::query(
            r#"
            UPDATE fanbase_connections
            SET last_sync_error = $3,
                last_sync_failed_at = now(),
                updated_at = now()
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(conn.workspace_id)
        .bind(conn.id)
        .bind(&trimmed)
        .execute(&self.pool)
        .await;
        if let Err(write_error) = written {
            tracing::warn!(
                connection_id = %conn.id,
                error = %write_error,
                "could not record a sync failure on the connection"
            );
        }
    }
}
