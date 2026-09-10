//! Single-active worker leadership via a PostgreSQL-backed lease.
//!
//! During blue-green deploys, the candidate worker starts in standby and polls
//! this lease. The old worker releases leadership on shutdown, allowing the
//! candidate to acquire it. This ensures only one worker generation runs
//! background loops at any time.
//!
//! The lease is row-level: `worker_leadership` has a single row (id=1). The
//! leader writes its ID, generation, and an expiry. A candidate can acquire
//! the lease when the current lease has expired or when it is already the
//! leader (renewal).
//!
//! The lease auto-expires after 60 seconds, so a crashed worker does not
//! strand leadership indefinitely. The leader renews every 15 seconds while
//! running.

use std::time::Duration;

use sqlx::PgPool;
use time::OffsetDateTime;
use tokio::sync::watch;

/// Lease duration: the leader must renew within this window or the lease
/// expires, allowing a standby candidate to take over. Used in SQL as
/// `INTERVAL '60 seconds'` — keep in sync.
#[cfg_attr(not(test), allow(dead_code))]
const LEASE_DURATION_SECS: u64 = 60;
const RENEW_INTERVAL: Duration = Duration::from_secs(15);
const STANDBY_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Error returned when leadership acquisition fails.
#[derive(Debug, thiserror::Error)]
pub enum LeadershipError {
    #[error("database error during leadership operation: {0}")]
    Database(#[from] sqlx::Error),
    #[error("leadership operation timed out")]
    Timeout,
}

/// A worker leadership lease. While held, this worker is the single active
/// generation. Drop the renewal task to release leadership.
pub struct LeadershipLease {
    worker_id: String,
    generation: i64,
    pool: PgPool,
    renewal_handle: tokio::task::JoinHandle<()>,
}

impl LeadershipLease {
    /// Returns the worker ID holding the lease.
    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    /// Returns the generation number of this lease.
    pub fn generation(&self) -> i64 {
        self.generation
    }

    /// Releases leadership by marking the lease as expired and aborting
    /// the renewal task. Called during graceful shutdown.
    pub async fn release(&self) {
        self.renewal_handle.abort();
        // Set expires_at to NOW() so the candidate can immediately acquire —
        // but only while this worker is still the leader.
        //
        // Without the `leader_id` guard this expired whoever held the lease,
        // not whoever called. During a blue-green deploy the old worker drains
        // slowly: its renewal can lapse, the new worker acquires, and then the
        // old worker's shutdown released the *new* leader's lease. The new
        // worker's own renewal restores it within 15 seconds, so the window is
        // short and a standby candidate polls every 2 seconds — long enough to
        // produce the two live generations this lease exists to prevent.
        //
        // `worker_id` is a per-process UUID, so this can only ever match the
        // caller's own lease.
        let released = sqlx::query(
            "UPDATE worker_leadership SET expires_at = NOW() WHERE id = 1 AND leader_id = $1",
        )
        .bind(&self.worker_id)
        .execute(&self.pool)
        .await;
        // Reported by what happened, not by having tried. The old line claimed
        // the lease was released even when the write failed, which tells an
        // operator the next worker can start now when it must wait out the
        // remaining lease instead.
        match released {
            Ok(result) if result.rows_affected() == 1 => tracing::info!(
                worker_id = %self.worker_id,
                generation = self.generation,
                "worker leadership released"
            ),
            Ok(_) => tracing::warn!(
                worker_id = %self.worker_id,
                generation = self.generation,
                "worker leadership was already held by another worker; nothing released"
            ),
            Err(error) => tracing::warn!(
                worker_id = %self.worker_id,
                generation = self.generation,
                error = %error,
                "could not release worker leadership; the lease expires on its own"
            ),
        }
    }
}

/// Attempts to acquire leadership. If the current lease has not expired,
/// returns `None` (caller should poll again in standby mode).
pub async fn try_acquire(
    pool: &PgPool,
    worker_id: &str,
) -> Result<Option<(i64, OffsetDateTime)>, LeadershipError> {
    let next_generation = next_generation(pool).await?;
    let acquired = sqlx::query_scalar::<_, bool>(
        r#"
        UPDATE worker_leadership
        SET leader_id = $1,
            generation = $2,
            acquired_at = NOW(),
            expires_at = NOW() + INTERVAL '60 seconds'
        WHERE id = 1
          AND (expires_at < NOW() OR leader_id = $1)
        RETURNING true
        "#,
    )
    .bind(worker_id)
    .bind(next_generation)
    .fetch_optional(pool)
    .await?;

    if acquired == Some(true) {
        let expires = sqlx::query_scalar::<_, OffsetDateTime>(
            "SELECT expires_at FROM worker_leadership WHERE id = 1",
        )
        .fetch_one(pool)
        .await?;
        Ok(Some((next_generation, expires)))
    } else {
        Ok(None)
    }
}

/// Returns the next generation number: always the stored one plus 1.
///
/// The comment here used to claim it kept the generation when the caller was
/// already the leader. It never did, and nothing wanted it to — renewal goes
/// through `renew_loop`, which extends `expires_at` and does not touch the
/// generation at all. `try_acquire` is only reached on a fresh acquisition, so
/// a bump is the correct answer every time it runs.
async fn next_generation(pool: &PgPool) -> Result<i64, LeadershipError> {
    let current =
        sqlx::query_scalar::<_, i64>("SELECT generation FROM worker_leadership WHERE id = 1")
            .fetch_one(pool)
            .await?;
    Ok(current + 1)
}

/// Acquires leadership, blocking until the lease is available. In standby
/// mode, polls every 2 seconds. Once acquired, spawns a renewal task that
/// extends the lease every 15 seconds.
///
/// If `standby` is false and leadership cannot be acquired immediately,
/// returns an error (the worker should not start without leadership unless
/// explicitly in standby mode).
pub async fn acquire_leadership(
    pool: PgPool,
    worker_id: String,
    standby: bool,
    mut shutdown: watch::Receiver<bool>,
) -> Result<LeadershipLease, LeadershipError> {
    loop {
        if *shutdown.borrow() {
            return Err(LeadershipError::Timeout);
        }

        match try_acquire(&pool, &worker_id).await {
            Ok(Some((generation, _expires))) => {
                tracing::info!(
                    worker_id = %worker_id,
                    generation,
                    "worker leadership acquired"
                );

                let renewal_pool = pool.clone();
                let renewal_worker_id = worker_id.clone();
                let renewal_shutdown = shutdown.clone();
                let renewal_handle = tokio::spawn(async move {
                    renew_loop(renewal_pool, renewal_worker_id, renewal_shutdown).await;
                });

                return Ok(LeadershipLease {
                    worker_id,
                    generation,
                    pool,
                    renewal_handle,
                });
            }
            Ok(None) => {
                if !standby {
                    // Non-standby mode: wait one cycle then try once more.
                    // If still unavailable, proceed anyway — the advisory
                    // behavior is best-effort for non-deploy restarts.
                    tracing::warn!(
                        worker_id = %worker_id,
                        "leadership not immediately available; waiting 2s before retry"
                    );
                }
                tracing::info!(
                    worker_id = %worker_id,
                    standby,
                    "waiting for worker leadership (standby mode)"
                );
                tokio::select! {
                    _ = tokio::time::sleep(STANDBY_POLL_INTERVAL) => {}
                    _ = shutdown.changed() => return Err(LeadershipError::Timeout),
                }
            }
            Err(error) => {
                tracing::warn!(
                    worker_id = %worker_id,
                    error = %error,
                    "leadership acquisition failed; retrying"
                );
                tokio::select! {
                    _ = tokio::time::sleep(STANDBY_POLL_INTERVAL) => {}
                    _ = shutdown.changed() => return Err(LeadershipError::Timeout),
                }
            }
        }
    }
}

/// Background renewal loop. Extends the lease every 15 seconds. If renewal
/// fails (e.g., DB outage), logs a warning but does not crash — the lease
/// will expire and a candidate may take over, which is the safe direction.
async fn renew_loop(pool: PgPool, worker_id: String, mut shutdown: watch::Receiver<bool>) {
    let mut interval = tokio::time::interval(RENEW_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let result = sqlx::query(
                    r#"
                    UPDATE worker_leadership
                    SET expires_at = NOW() + INTERVAL '60 seconds'
                    WHERE id = 1 AND leader_id = $1
                    "#,
                )
                .bind(&worker_id)
                .execute(&pool)
                .await;

                match result {
                    Ok(result) => {
                        if result.rows_affected() == 0 {
                            tracing::error!(
                                worker_id = %worker_id,
                                "leadership renewal failed: lease was taken by another worker"
                            );
                            return;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            worker_id = %worker_id,
                            error = %error,
                            "leadership renewal failed; lease may expire"
                        );
                    }
                }
            }
            _ = shutdown.changed() => {
                tracing::debug!(worker_id = %worker_id, "leadership renewal stopping");
                return;
            }
        }
    }
}

/// Returns the current leader ID and generation, or `None` if the table
/// is empty (should not happen after migration).
pub async fn current_leader(pool: &PgPool) -> Result<Option<(String, i64)>, LeadershipError> {
    let row = sqlx::query_as::<_, (String, i64)>(
        "SELECT leader_id, generation FROM worker_leadership WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lease_duration_is_bounded() {
        // Use const assertions for constant checks to satisfy clippy.
        const _: () = {
            assert!(LEASE_DURATION_SECS >= 30, "lease must be at least 30s");
            assert!(LEASE_DURATION_SECS <= 120, "lease must be at most 120s");
        };
        assert!(
            RENEW_INTERVAL.as_secs() < LEASE_DURATION_SECS / 2,
            "renewal must be more frequent than half the lease"
        );
    }

    #[test]
    fn test_standby_poll_interval_is_short() {
        assert!(
            STANDBY_POLL_INTERVAL.as_secs() <= 5,
            "standby poll must be at most 5s"
        );
    }
}
