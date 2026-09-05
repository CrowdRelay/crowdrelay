//! The executor circuit breaker: what consecutive executor failures cost.
//!
//! Its own module rather than two SQL blobs inside `record_execution_report`.
//! This is a statement about an *executor's* health, not about any one action's
//! outcome, and the receipt path is only one of the things that could report it.
//!
//! Both statements are ordered by `last_failure_at`, so an out-of-order receipt
//! — the normal case with an at-least-once transport — cannot rewind the
//! breaker with a stale observation.

use super::*;

/// Consecutive failures before the breaker guards the executor.
const TRIP_THRESHOLD: i32 = 3;

/// How long failures count as consecutive, and how long a tripped breaker
/// guards for.
const WINDOW: &str = "15 minutes";

/// Records an executor failure, tripping the breaker on the third consecutive
/// one.
///
/// "Consecutive" means within [`WINDOW`] of the previous failure; an older
/// failure restarts the count at one. Tripping extends `guarded_until` rather
/// than replacing it, so a burst cannot shorten a guard already in place.
pub(super) async fn record_failure(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    executor_id: &str,
    occurred_at: OffsetDateTime,
    reason: &str,
) -> Result<(), RepositoryError> {
    // The consecutive-failure count is needed three times in the statement —
    // to store, to decide the guard, and to decide the reason — and writing it
    // out three times is how the three drift apart.
    let consecutive = format!(
        "(CASE WHEN viryaos_executor_circuit_breakers.last_failure_at \
           >= EXCLUDED.last_failure_at - INTERVAL '{WINDOW}' \
         THEN viryaos_executor_circuit_breakers.failure_count + 1 ELSE 1 END)"
    );
    let statement = format!(
        r#"
        INSERT INTO viryaos_executor_circuit_breakers (
            workspace_id, executor_id, failure_count, last_failure_at, reason
        ) VALUES ($1,$2,1,$3,$4)
        ON CONFLICT (workspace_id, executor_id) DO UPDATE
        SET failure_count = {consecutive},
            last_failure_at = EXCLUDED.last_failure_at,
            guarded_until = CASE
                WHEN {consecutive} >= {TRIP_THRESHOLD}
                THEN GREATEST(
                    COALESCE(viryaos_executor_circuit_breakers.guarded_until, EXCLUDED.last_failure_at),
                    EXCLUDED.last_failure_at + INTERVAL '{WINDOW}'
                )
                WHEN viryaos_executor_circuit_breakers.guarded_until > EXCLUDED.last_failure_at
                THEN viryaos_executor_circuit_breakers.guarded_until
                ELSE NULL END,
            reason = CASE
                WHEN {consecutive} >= {TRIP_THRESHOLD}
                THEN EXCLUDED.reason
                ELSE viryaos_executor_circuit_breakers.reason END
        WHERE viryaos_executor_circuit_breakers.last_failure_at IS NULL
           OR viryaos_executor_circuit_breakers.last_failure_at <= EXCLUDED.last_failure_at
        "#
    );
    sqlx::query(&statement)
        .bind(workspace_id.into_uuid())
        .bind(executor_id)
        .bind(occurred_at)
        .bind(reason)
        .execute(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
    Ok(())
}

/// Clears the breaker after a success, unless the stored failure is newer than
/// this success — a late success receipt must not clear a failure that came
/// after it.
pub(super) async fn record_success(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    executor_id: &str,
    occurred_at: OffsetDateTime,
) -> Result<(), RepositoryError> {
    sqlx::query(
        r#"
        UPDATE viryaos_executor_circuit_breakers
        SET failure_count=0, last_failure_at=NULL, guarded_until=NULL, reason=NULL
        WHERE workspace_id=$1 AND executor_id=$2
          AND (last_failure_at IS NULL OR last_failure_at <= $3)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(executor_id)
    .bind(occurred_at)
    .execute(&mut **transaction)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}
