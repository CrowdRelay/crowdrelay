//! Deletes venue facts whose licence clock has run out (§12-2, 4V.8).
//!
//! `expires_at` on `place_venue_facts` is a deletion deadline, not a
//! staleness hint: a licensed `commercial_directory` claim is data we hold
//! on borrowed time, and "filtered at read time but still on disk" is not
//! what the licence says. This worker enforces it — one statement, once an
//! hour, and the read model's `expires_at > now()` filter only ever sees a
//! row whose deadline passed since the last sweep.
//!
//! The sweep is deliberately unscoped: expiry applies to global and
//! contributor-private facts alike — a licence clock does not stop being a
//! licence clock because the fact was one tenant's knowledge. That makes
//! this one of the few queries that legitimately names no `workspace_id`,
//! and the workspace-scope ratchet records it as such.

use std::time::Duration;

use sqlx::PgPool;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};

pub struct VenueFactExpiryWorker {
    pool: PgPool,
    sweep_interval: Duration,
    operation_timeout: Duration,
}

impl VenueFactExpiryWorker {
    #[must_use]
    pub fn new(pool: PgPool, sweep_interval: Duration, operation_timeout: Duration) -> Self {
        Self {
            pool,
            sweep_interval,
            operation_timeout,
        }
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = interval(self.sweep_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
                _ = ticker.tick() => {
                    match timeout(self.operation_timeout, delete_expired_facts(&self.pool)).await {
                        Ok(Ok(deleted)) => {
                            if deleted > 0 {
                                tracing::info!(deleted, "expired venue facts deleted");
                            }
                        }
                        Ok(Err(error)) => {
                            tracing::warn!(%error, "venue fact expiry sweep failed");
                        }
                        Err(_) => {
                            tracing::warn!("venue fact expiry sweep timed out");
                        }
                    }
                }
            }
        }
    }
}

/// One `DELETE` over every expired fact. Returns how many rows died — the
/// count the caller logs, so a quiet zero and a real purge read differently.
///
/// # Errors
///
/// Propagates the database error.
pub async fn delete_expired_facts(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM place_venue_facts WHERE expires_at IS NOT NULL AND expires_at < now()",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}
