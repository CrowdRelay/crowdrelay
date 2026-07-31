//! Bounded cleanup of expired operational data and delivered secret material.
//!
//! Every mutation is limited with `FOR UPDATE SKIP LOCKED`, runs inside one
//! transaction with PostgreSQL statement/lock timeouts, and leaves durable
//! business records such as consent and audit events untouched.

use std::time::Duration;

use sqlx::{PgPool, Postgres, Transaction};
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60 * 60);
const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_TERMINAL_OUTBOX_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const DEFAULT_CONSUMED_TOKEN_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const DEFAULT_BATCH_SIZE: u32 = 1_000;
const MAX_BATCH_SIZE: u32 = 1_000;

/// Runtime bounds for one retention worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionWorkerConfig {
    pub poll_interval: Duration,
    pub operation_timeout: Duration,
    pub lock_timeout: Duration,
    pub terminal_outbox_retention: Duration,
    pub consumed_token_retention: Duration,
    pub batch_size: u32,
}

impl Default for RetentionWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            operation_timeout: DEFAULT_OPERATION_TIMEOUT,
            lock_timeout: DEFAULT_LOCK_TIMEOUT,
            terminal_outbox_retention: DEFAULT_TERMINAL_OUTBOX_RETENTION,
            consumed_token_retention: DEFAULT_CONSUMED_TOKEN_RETENTION,
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }
}

/// Periodic, concurrency-safe cleanup of short-lived operational records.
#[derive(Clone, Debug)]
pub struct RetentionWorker {
    pool: PgPool,
    poll_interval: Duration,
    operation_timeout: Duration,
    statement_timeout_ms: i64,
    lock_timeout_ms: i64,
    terminal_outbox_retention_ms: i64,
    consumed_token_retention_ms: i64,
    batch_size: i64,
}

impl RetentionWorker {
    pub fn new(
        pool: PgPool,
        config: RetentionWorkerConfig,
    ) -> Result<Self, RetentionWorkerBuildError> {
        validate_config(config)?;
        Ok(Self {
            pool,
            poll_interval: config.poll_interval,
            operation_timeout: config.operation_timeout,
            statement_timeout_ms: duration_milliseconds(config.operation_timeout)?,
            lock_timeout_ms: duration_milliseconds(config.lock_timeout)?,
            terminal_outbox_retention_ms: duration_milliseconds(config.terminal_outbox_retention)?,
            consumed_token_retention_ms: duration_milliseconds(config.consumed_token_retention)?,
            batch_size: i64::from(config.batch_size),
        })
    }

    /// Runs an immediate first cycle, then one cycle per configured interval,
    /// until shutdown is requested.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        if *shutdown.borrow() {
            return;
        }
        let mut ticker = interval(self.poll_interval);
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
                    match self.run_once().await {
                        Ok(stats) if stats.did_work() => {
                            tracing::info!(
                                idempotency_keys_deleted = stats.idempotency_keys_deleted,
                                webhook_replay_keys_deleted = stats.webhook_replay_keys_deleted,
                                fan_sessions_deleted = stats.fan_sessions_deleted,
                                pass_sessions_deleted = stats.pass_sessions_deleted,
                                member_sessions_deleted = stats.member_sessions_deleted,
                                fan_action_tokens_deleted = stats.fan_action_tokens_deleted,
                                expired_admission_passes_reconciled =
                                    stats.expired_admission_passes_reconciled,
                                outbox_payloads_scrubbed = stats.outbox_payloads_scrubbed,
                                terminal_outbox_events_deleted =
                                    stats.terminal_outbox_events_deleted,
                                "retention cycle completed"
                            );
                        }
                        Ok(_) => {}
                        Err(error) => {
                            tracing::warn!(
                                error_kind = error.kind(),
                                "retention cycle could not complete"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Executes one transaction without waiting for the periodic tick.
    pub async fn run_once(&self) -> Result<RetentionStats, RetentionRunError> {
        self.run_cycle().await
    }

    async fn run_cycle(&self) -> Result<RetentionStats, RetentionRunError> {
        let mut stats = RetentionStats::default();
        let mut first_failure = None;

        macro_rules! execute_step {
            ($field:ident, $step:expr) => {
                match self.bounded_step($step).await {
                    Ok(changed) => stats.$field = changed,
                    Err(error) => {
                        tracing::warn!(
                            retention_step = $step.as_str(),
                            error_kind = error.kind(),
                            "retention step could not complete"
                        );
                        if first_failure.is_none() {
                            first_failure = Some(error);
                        }
                    }
                }
            };
        }

        execute_step!(
            idempotency_keys_deleted,
            RetentionStep::ExpiredIdempotencyKeys
        );
        execute_step!(
            webhook_replay_keys_deleted,
            RetentionStep::ExpiredWebhookReplayKeys
        );
        execute_step!(fan_sessions_deleted, RetentionStep::TerminalFanSessions);
        execute_step!(pass_sessions_deleted, RetentionStep::TerminalPassSessions);
        execute_step!(
            member_sessions_deleted,
            RetentionStep::TerminalMemberSessions
        );
        execute_step!(
            fan_action_tokens_deleted,
            RetentionStep::TerminalFanActionTokens
        );
        execute_step!(
            expired_admission_passes_reconciled,
            RetentionStep::ExpiredAdmissionPasses
        );
        execute_step!(
            terminal_outbox_events_deleted,
            RetentionStep::OldTerminalOutboxEvents
        );
        execute_step!(
            outbox_payloads_scrubbed,
            RetentionStep::TerminalOutboxSecrets
        );

        if let Some(error) = first_failure {
            return Err(error);
        }
        Ok(stats)
    }

    async fn bounded_step(&self, step: RetentionStep) -> Result<u64, RetentionRunError> {
        timeout(self.operation_timeout, self.run_step(step))
            .await
            .map_err(|_| RetentionRunError::Timeout)?
    }

    async fn run_step(&self, step: RetentionStep) -> Result<u64, RetentionRunError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(RetentionRunError::Database)?;
        configure_transaction(
            &mut transaction,
            self.statement_timeout_ms,
            self.lock_timeout_ms,
        )
        .await?;

        let changed = match step {
            RetentionStep::ExpiredIdempotencyKeys => {
                delete_expired_idempotency_keys(&mut transaction, self.batch_size).await?
            }
            RetentionStep::ExpiredWebhookReplayKeys => {
                delete_expired_webhook_replay_keys(&mut transaction, self.batch_size).await?
            }
            RetentionStep::TerminalFanSessions => {
                delete_terminal_fan_sessions(&mut transaction, self.batch_size).await?
            }
            RetentionStep::TerminalPassSessions => {
                delete_terminal_pass_sessions(&mut transaction, self.batch_size).await?
            }
            RetentionStep::TerminalMemberSessions => {
                delete_terminal_member_sessions(&mut transaction, self.batch_size).await?
            }
            RetentionStep::TerminalFanActionTokens => {
                delete_terminal_fan_action_tokens(
                    &mut transaction,
                    self.batch_size,
                    self.consumed_token_retention_ms,
                )
                .await?
            }
            RetentionStep::ExpiredAdmissionPasses => {
                reconcile_expired_admission_passes(&mut transaction, self.batch_size).await?
            }
            RetentionStep::OldTerminalOutboxEvents => {
                delete_old_terminal_outbox_events(
                    &mut transaction,
                    self.batch_size,
                    self.terminal_outbox_retention_ms,
                )
                .await?
            }
            RetentionStep::TerminalOutboxSecrets => {
                scrub_terminal_outbox_secrets(&mut transaction, self.batch_size).await?
            }
        };

        transaction
            .commit()
            .await
            .map_err(RetentionRunError::Database)?;
        Ok(changed)
    }
}

#[derive(Clone, Copy)]
enum RetentionStep {
    ExpiredIdempotencyKeys,
    ExpiredWebhookReplayKeys,
    TerminalFanSessions,
    TerminalPassSessions,
    TerminalMemberSessions,
    TerminalFanActionTokens,
    ExpiredAdmissionPasses,
    OldTerminalOutboxEvents,
    TerminalOutboxSecrets,
}

impl RetentionStep {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ExpiredIdempotencyKeys => "expired_idempotency_keys",
            Self::ExpiredWebhookReplayKeys => "expired_webhook_replay_keys",
            Self::TerminalFanSessions => "terminal_fan_sessions",
            Self::TerminalPassSessions => "terminal_pass_sessions",
            Self::TerminalMemberSessions => "terminal_member_sessions",
            Self::TerminalFanActionTokens => "terminal_fan_action_tokens",
            Self::ExpiredAdmissionPasses => "expired_admission_passes",
            Self::OldTerminalOutboxEvents => "old_terminal_outbox_events",
            Self::TerminalOutboxSecrets => "terminal_outbox_secrets",
        }
    }
}

/// Counts of rows changed by one committed retention transaction.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RetentionStats {
    pub idempotency_keys_deleted: u64,
    pub webhook_replay_keys_deleted: u64,
    pub fan_sessions_deleted: u64,
    pub pass_sessions_deleted: u64,
    pub member_sessions_deleted: u64,
    pub fan_action_tokens_deleted: u64,
    pub expired_admission_passes_reconciled: u64,
    pub outbox_payloads_scrubbed: u64,
    pub terminal_outbox_events_deleted: u64,
}

impl RetentionStats {
    #[must_use]
    pub fn did_work(self) -> bool {
        self.idempotency_keys_deleted > 0
            || self.webhook_replay_keys_deleted > 0
            || self.fan_sessions_deleted > 0
            || self.pass_sessions_deleted > 0
            || self.member_sessions_deleted > 0
            || self.fan_action_tokens_deleted > 0
            || self.expired_admission_passes_reconciled > 0
            || self.outbox_payloads_scrubbed > 0
            || self.terminal_outbox_events_deleted > 0
    }
}

fn validate_config(config: RetentionWorkerConfig) -> Result<(), RetentionWorkerBuildError> {
    if config.poll_interval.is_zero()
        || config.operation_timeout.is_zero()
        || config.lock_timeout.is_zero()
        || config.terminal_outbox_retention.is_zero()
        || config.consumed_token_retention.is_zero()
    {
        return Err(RetentionWorkerBuildError::ZeroDuration);
    }
    if config.lock_timeout > config.operation_timeout {
        return Err(RetentionWorkerBuildError::InvalidTimeoutOrder);
    }
    if !(1..=MAX_BATCH_SIZE).contains(&config.batch_size) {
        return Err(RetentionWorkerBuildError::InvalidBatchSize);
    }
    for value in [
        config.poll_interval,
        config.operation_timeout,
        config.lock_timeout,
        config.terminal_outbox_retention,
        config.consumed_token_retention,
    ] {
        duration_milliseconds(value)?;
    }
    Ok(())
}

fn duration_milliseconds(value: Duration) -> Result<i64, RetentionWorkerBuildError> {
    i64::try_from(value.as_millis()).map_err(|_| RetentionWorkerBuildError::DurationOverflow)
}

async fn configure_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    statement_timeout_ms: i64,
    lock_timeout_ms: i64,
) -> Result<(), RetentionRunError> {
    sqlx::query(
        "SELECT set_config('statement_timeout', $1, true), set_config('lock_timeout', $2, true)",
    )
    .bind(format!("{statement_timeout_ms}ms"))
    .bind(format!("{lock_timeout_ms}ms"))
    .execute(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    Ok(())
}

async fn delete_expired_idempotency_keys(
    transaction: &mut Transaction<'_, Postgres>,
    batch_size: i64,
) -> Result<u64, RetentionRunError> {
    let result = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT idem.workspace_id, idem.scope, idem.key
            FROM idempotency_keys AS idem
            WHERE idem.expires_at <= now()
            ORDER BY idem.expires_at, idem.workspace_id, idem.scope, idem.key
            FOR UPDATE OF idem SKIP LOCKED
            LIMIT $1
        )
        DELETE FROM idempotency_keys AS idem
        USING candidates
        WHERE idem.workspace_id = candidates.workspace_id
            AND idem.scope = candidates.scope
            AND idem.key = candidates.key
        "#,
    )
    .bind(batch_size)
    .execute(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    Ok(result.rows_affected())
}

async fn delete_expired_webhook_replay_keys(
    transaction: &mut Transaction<'_, Postgres>,
    batch_size: i64,
) -> Result<u64, RetentionRunError> {
    let result = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT replay.workspace_id, replay.source, replay.event_id
            FROM webhook_replay_keys AS replay
            WHERE replay.expires_at <= now()
            ORDER BY replay.expires_at, replay.workspace_id, replay.source, replay.event_id
            FOR UPDATE OF replay SKIP LOCKED
            LIMIT $1
        )
        DELETE FROM webhook_replay_keys AS replay
        USING candidates
        WHERE replay.workspace_id = candidates.workspace_id
            AND replay.source = candidates.source
            AND replay.event_id = candidates.event_id
        "#,
    )
    .bind(batch_size)
    .execute(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    Ok(result.rows_affected())
}

async fn delete_terminal_fan_sessions(
    transaction: &mut Transaction<'_, Postgres>,
    batch_size: i64,
) -> Result<u64, RetentionRunError> {
    let result = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT session.id
            FROM fan_sessions AS session
            WHERE LEAST(
                session.expires_at,
                COALESCE(session.revoked_at, session.expires_at)
            ) <= now()
            ORDER BY
                LEAST(
                    session.expires_at,
                    COALESCE(session.revoked_at, session.expires_at)
                ),
                session.id
            FOR UPDATE OF session SKIP LOCKED
            LIMIT $1
        )
        DELETE FROM fan_sessions AS session
        USING candidates
        WHERE session.id = candidates.id
        "#,
    )
    .bind(batch_size)
    .execute(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    Ok(result.rows_affected())
}

async fn delete_terminal_pass_sessions(
    transaction: &mut Transaction<'_, Postgres>,
    batch_size: i64,
) -> Result<u64, RetentionRunError> {
    let result = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT session.id
            FROM pass_sessions AS session
            WHERE LEAST(
                session.expires_at,
                COALESCE(session.revoked_at, session.expires_at)
            ) <= now()
            ORDER BY
                LEAST(
                    session.expires_at,
                    COALESCE(session.revoked_at, session.expires_at)
                ),
                session.id
            FOR UPDATE OF session SKIP LOCKED
            LIMIT $1
        )
        DELETE FROM pass_sessions AS session
        USING candidates
        WHERE session.id = candidates.id
        "#,
    )
    .bind(batch_size)
    .execute(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    Ok(result.rows_affected())
}

async fn delete_terminal_member_sessions(
    transaction: &mut Transaction<'_, Postgres>,
    batch_size: i64,
) -> Result<u64, RetentionRunError> {
    let result = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT session.id
            FROM workspace_member_sessions AS session
            WHERE LEAST(
                session.expires_at,
                COALESCE(session.revoked_at, session.expires_at)
            ) <= now()
            AND NOT EXISTS (
                SELECT 1
                FROM pass_redemptions AS redemption
                WHERE redemption.workspace_id = session.workspace_id
                    AND redemption.staff_session_id = session.id
            )
            ORDER BY
                LEAST(
                    session.expires_at,
                    COALESCE(session.revoked_at, session.expires_at)
                ),
                session.id
            FOR UPDATE OF session SKIP LOCKED
            LIMIT $1
        )
        DELETE FROM workspace_member_sessions AS session
        USING candidates
        WHERE session.id = candidates.id
        "#,
    )
    .bind(batch_size)
    .execute(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    Ok(result.rows_affected())
}

async fn delete_terminal_fan_action_tokens(
    transaction: &mut Transaction<'_, Postgres>,
    batch_size: i64,
    consumed_token_retention_ms: i64,
) -> Result<u64, RetentionRunError> {
    let result = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT token.id
            FROM fan_action_tokens AS token
            WHERE token.expires_at <= now()
                OR token.consumed_at <=
                    now() - ($2::bigint * interval '1 millisecond')
            ORDER BY
                LEAST(
                    token.expires_at,
                    COALESCE(token.consumed_at, token.expires_at)
                ),
                token.id
            FOR UPDATE OF token SKIP LOCKED
            LIMIT $1
        )
        DELETE FROM fan_action_tokens AS token
        USING candidates
        WHERE token.id = candidates.id
        "#,
    )
    .bind(batch_size)
    .bind(consumed_token_retention_ms)
    .execute(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    Ok(result.rows_affected())
}

async fn reconcile_expired_admission_passes(
    transaction: &mut Transaction<'_, Postgres>,
    batch_size: i64,
) -> Result<u64, RetentionRunError> {
    let (expired_count, released_capacity) = sqlx::query_as::<_, (i64, i64)>(
        r#"
        WITH candidates AS (
            SELECT
                pass.id,
                pass.workspace_id,
                pass.admission_pool_id
            FROM admission_passes AS pass
            WHERE pass.status = 'issued'
                AND pass.claim_expires_at <= now()
            ORDER BY pass.claim_expires_at, pass.id
            FOR UPDATE OF pass SKIP LOCKED
            LIMIT $1
        ),
        expired AS (
            UPDATE admission_passes AS pass
            SET
                status = 'expired',
                claim_token_hash = NULL
            FROM candidates
            WHERE pass.workspace_id = candidates.workspace_id
                AND pass.id = candidates.id
                AND pass.status = 'issued'
                AND pass.claim_expires_at <= now()
            RETURNING pass.workspace_id, pass.admission_pool_id
        ),
        decrements AS (
            SELECT
                expired.workspace_id,
                expired.admission_pool_id,
                count(*)::bigint AS released_count
            FROM expired
            GROUP BY expired.workspace_id, expired.admission_pool_id
        ),
        updated_pools AS (
            UPDATE admission_pools AS pool
            SET issued_count =
                pool.issued_count - decrements.released_count::integer
            FROM decrements
            WHERE pool.workspace_id = decrements.workspace_id
                AND pool.id = decrements.admission_pool_id
                AND pool.issued_count >= decrements.released_count
            RETURNING decrements.released_count
        )
        SELECT
            (SELECT count(*)::bigint FROM expired),
            COALESCE(
                (SELECT sum(updated_pools.released_count)::bigint FROM updated_pools),
                0
            )
        "#,
    )
    .bind(batch_size)
    .fetch_one(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    if expired_count != released_capacity {
        return Err(RetentionRunError::Invariant);
    }
    u64::try_from(expired_count).map_err(|_| RetentionRunError::Invariant)
}

async fn delete_old_terminal_outbox_events(
    transaction: &mut Transaction<'_, Postgres>,
    batch_size: i64,
    terminal_outbox_retention_ms: i64,
) -> Result<u64, RetentionRunError> {
    // Deleting the parent cascades its terminal deliveries and attempt rows.
    // Standalone delivery deletion would break materialization idempotency while
    // the parent event is retained, so it is intentionally not performed.
    let result = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT event.id
            FROM outbox_events AS event
            WHERE event.status IN ('delivered', 'dead')
                AND COALESCE(event.delivered_at, event.dead_at) <=
                    now() - ($2::bigint * interval '1 millisecond')
                AND NOT EXISTS (
                    SELECT 1
                    FROM webhook_deliveries AS delivery
                    WHERE delivery.workspace_id = event.workspace_id
                        AND delivery.outbox_event_id = event.id
                        AND delivery.status IN ('pending', 'processing')
                )
            ORDER BY COALESCE(event.delivered_at, event.dead_at), event.id
            FOR UPDATE OF event SKIP LOCKED
            LIMIT $1
        )
        DELETE FROM outbox_events AS event
        USING candidates
        WHERE event.id = candidates.id
        "#,
    )
    .bind(batch_size)
    .bind(terminal_outbox_retention_ms)
    .execute(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    Ok(result.rows_affected())
}

async fn scrub_terminal_outbox_secrets(
    transaction: &mut Transaction<'_, Postgres>,
    batch_size: i64,
) -> Result<u64, RetentionRunError> {
    let result = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT event.id
            FROM outbox_events AS event
            WHERE event.status IN ('delivered', 'dead')
                AND event.payload ?| ARRAY[
                    'confirmation_token',
                    'session_recovery_token',
                    'unsubscribe_token',
                    'claim_token',
                    'coupon_code'
                ]
                AND NOT EXISTS (
                    SELECT 1
                    FROM webhook_deliveries AS delivery
                    WHERE delivery.workspace_id = event.workspace_id
                        AND delivery.outbox_event_id = event.id
                        AND delivery.status IN ('pending', 'processing')
                )
            ORDER BY COALESCE(event.delivered_at, event.dead_at), event.id
            FOR UPDATE OF event SKIP LOCKED
            LIMIT $1
        )
        UPDATE outbox_events AS event
        SET payload = event.payload - ARRAY[
            'confirmation_token',
            'session_recovery_token',
            'unsubscribe_token',
            'claim_token',
            'coupon_code'
        ]::text[]
        FROM candidates
        WHERE event.id = candidates.id
        "#,
    )
    .bind(batch_size)
    .execute(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    Ok(result.rows_affected())
}

/// Invalid construction parameters for the periodic retention worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RetentionWorkerBuildError {
    #[error("retention durations must be non-zero")]
    ZeroDuration,
    #[error("retention lock timeout must not exceed its operation timeout")]
    InvalidTimeoutOrder,
    #[error("retention batch size must be between 1 and 1000")]
    InvalidBatchSize,
    #[error("retention duration exceeds PostgreSQL millisecond limits")]
    DurationOverflow,
}

/// Sanitized failure from one retention cycle.
#[derive(Debug, thiserror::Error)]
pub enum RetentionRunError {
    #[error("retention database operation failed")]
    Database(#[source] sqlx::Error),
    #[error("retention cycle timed out")]
    Timeout,
    #[error("retention detected inconsistent admission pool accounting")]
    Invariant,
}

impl RetentionRunError {
    const fn kind(&self) -> &'static str {
        match self {
            Self::Database(_) => "database",
            Self::Timeout => "timeout",
            Self::Invariant => "invariant",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use sqlx::{PgPool, postgres::PgPoolOptions};
    use uuid::Uuid;

    use super::{
        MAX_BATCH_SIZE, RetentionStats, RetentionWorker, RetentionWorkerBuildError,
        RetentionWorkerConfig,
    };

    fn lazy_pool() -> Result<PgPool, sqlx::Error> {
        PgPoolOptions::new().connect_lazy("postgres://crowdrelay:crowdrelay@localhost/crowdrelay")
    }

    #[tokio::test]
    async fn default_configuration_is_valid_and_bounded() -> Result<(), Box<dyn std::error::Error>>
    {
        RetentionWorker::new(lazy_pool()?, RetentionWorkerConfig::default())?;
        RetentionWorker::new(
            lazy_pool()?,
            RetentionWorkerConfig {
                batch_size: MAX_BATCH_SIZE,
                ..RetentionWorkerConfig::default()
            },
        )?;
        Ok(())
    }

    #[tokio::test]
    async fn rejects_zero_durations() -> Result<(), Box<dyn std::error::Error>> {
        let defaults = RetentionWorkerConfig::default();
        for config in [
            RetentionWorkerConfig {
                poll_interval: Duration::ZERO,
                ..defaults
            },
            RetentionWorkerConfig {
                operation_timeout: Duration::ZERO,
                ..defaults
            },
            RetentionWorkerConfig {
                lock_timeout: Duration::ZERO,
                ..defaults
            },
            RetentionWorkerConfig {
                terminal_outbox_retention: Duration::ZERO,
                ..defaults
            },
            RetentionWorkerConfig {
                consumed_token_retention: Duration::ZERO,
                ..defaults
            },
        ] {
            assert_eq!(
                RetentionWorker::new(lazy_pool()?, config).expect_err("config must be rejected"),
                RetentionWorkerBuildError::ZeroDuration
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn rejects_unbounded_batch_and_invalid_timeout_order()
    -> Result<(), Box<dyn std::error::Error>> {
        for batch_size in [0, MAX_BATCH_SIZE + 1] {
            let error = RetentionWorker::new(
                lazy_pool()?,
                RetentionWorkerConfig {
                    batch_size,
                    ..RetentionWorkerConfig::default()
                },
            )
            .expect_err("batch must be rejected");
            assert_eq!(error, RetentionWorkerBuildError::InvalidBatchSize);
        }

        let error = RetentionWorker::new(
            lazy_pool()?,
            RetentionWorkerConfig {
                operation_timeout: Duration::from_secs(1),
                lock_timeout: Duration::from_secs(2),
                ..RetentionWorkerConfig::default()
            },
        )
        .expect_err("timeout order must be rejected");
        assert_eq!(error, RetentionWorkerBuildError::InvalidTimeoutOrder);
        Ok(())
    }

    #[tokio::test]
    async fn rejects_duration_that_postgres_cannot_represent()
    -> Result<(), Box<dyn std::error::Error>> {
        let error = RetentionWorker::new(
            lazy_pool()?,
            RetentionWorkerConfig {
                terminal_outbox_retention: Duration::MAX,
                ..RetentionWorkerConfig::default()
            },
        )
        .expect_err("overflow must be rejected");
        assert_eq!(error, RetentionWorkerBuildError::DurationOverflow);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires CROWDRELAY_RETENTION_TEST_DATABASE_URL and a disposable PostgreSQL database"]
    async fn cycle_deletes_expired_rows_scrubs_safe_payloads_and_preserves_audit()
    -> Result<(), Box<dyn std::error::Error>> {
        let database_url =
            std::env::var("CROWDRELAY_RETENTION_TEST_DATABASE_URL").map_err(|e| {
                format!(
                    "CROWDRELAY_RETENTION_TEST_DATABASE_URL must target a disposable database: {e}"
                )
            })?;
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await?;
        crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

        let suffix = Uuid::now_v7().simple().to_string();
        let workspace_id = Uuid::now_v7();
        let fan_id = Uuid::now_v7();
        let event_id = Uuid::now_v7();
        let admission_pool_id = Uuid::now_v7();
        let expired_pass_id = Uuid::now_v7();
        let expired_session_id = Uuid::now_v7();
        let expired_action_id = Uuid::now_v7();
        let recent_consumed_action_id = Uuid::now_v7();
        let endpoint_id = Uuid::now_v7();
        let scrubbed_event_id = Uuid::now_v7();
        let blocked_event_id = Uuid::now_v7();
        let deleted_event_id = Uuid::now_v7();
        let blocked_delivery_id = Uuid::now_v7();
        let deleted_delivery_id = Uuid::now_v7();
        let expired_idempotency_key = format!("expired-{suffix}");
        let retained_idempotency_key = format!("retained-{suffix}");
        let expired_replay_id = format!("expired-replay-{suffix}");

        sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Retention test')")
            .bind(workspace_id)
            .bind(format!("retention-{suffix}"))
            .execute(&pool)
            .await?;
        sqlx::query(
            r#"
            INSERT INTO fans (
                id, workspace_id, normalized_email, display_name, locale, status
            )
            VALUES ($1, $2, $3, 'Retention fan', 'pl-PL', 'active')
            "#,
        )
        .bind(fan_id)
        .bind(workspace_id)
        .bind(format!("retention-{suffix}@example.test"))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO fan_consents (
                workspace_id, fan_id, purpose, granted, policy_version, source
            )
            VALUES ($1, $2, 'marketing', true, 'retention-v1', 'retention-test')
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO audit_events (
                workspace_id, actor_kind, action, target_type, target_id
            )
            VALUES ($1, 'system', 'retention.test', 'fan', $2)
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id.to_string())
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO events (
                id, workspace_id, slug, title, starts_at, status, published_at
            )
            VALUES (
                $1, $2, $3, 'Retention event',
                now() + interval '30 days', 'published', now()
            )
            "#,
        )
        .bind(event_id)
        .bind(workspace_id)
        .bind(format!("retention-event-{suffix}"))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO admission_pools (
                id, workspace_id, event_id, name, slug, capacity, issued_count
            )
            VALUES ($1, $2, $3, 'Retention pool', $4, 10, 1)
            "#,
        )
        .bind(admission_pool_id)
        .bind(workspace_id)
        .bind(event_id)
        .bind(format!("retention-pool-{suffix}"))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO admission_passes (
                id, workspace_id, event_id, admission_pool_id, fan_id,
                issuance_method, public_reference, claim_token_hash,
                claim_expires_at, status, issued_at
            )
            VALUES (
                $1, $2, $3, $4, $5,
                'first_come', $6, digest($7, 'sha256'),
                now() - interval '1 day', 'issued',
                now() - interval '2 days'
            )
            "#,
        )
        .bind(expired_pass_id)
        .bind(workspace_id)
        .bind(event_id)
        .bind(admission_pool_id)
        .bind(fan_id)
        .bind(format!("RETENTION-{suffix}"))
        .bind(format!("expired-claim-{suffix}"))
        .execute(&pool)
        .await?;

        sqlx::query(
            r#"
            INSERT INTO idempotency_keys (
                workspace_id, scope, key, request_hash, state, response_status,
                response_body, response_content_type, created_at, completed_at, expires_at
            )
            VALUES (
                $1, 'retention-test', $2, digest($3, 'sha256'), 'completed', 200,
                '{}'::jsonb, 'application/json',
                now() - interval '3 days',
                now() - interval '2 days',
                now() - interval '1 day'
            )
            "#,
        )
        .bind(workspace_id)
        .bind(&expired_idempotency_key)
        .bind(format!("request-expired-{suffix}"))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO idempotency_keys (
                workspace_id, scope, key, request_hash, state, response_status,
                response_body, response_content_type, completed_at, expires_at
            )
            VALUES (
                $1, 'retention-test', $2, digest($3, 'sha256'), 'completed', 200,
                '{}'::jsonb, 'application/json', now(), now() + interval '1 day'
            )
            "#,
        )
        .bind(workspace_id)
        .bind(&retained_idempotency_key)
        .bind(format!("request-retained-{suffix}"))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO webhook_replay_keys (
                workspace_id, source, event_id, body_sha256,
                signed_at, received_at, expires_at
            )
            VALUES (
                $1, 'retention-test', $2, digest($3, 'sha256'),
                now() - interval '2 days',
                now() - interval '2 days',
                now() - interval '1 day'
            )
            "#,
        )
        .bind(workspace_id)
        .bind(&expired_replay_id)
        .bind(format!("replay-body-{suffix}"))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO fan_sessions (
                id, workspace_id, fan_id, session_token_hash,
                created_at, last_seen_at, expires_at
            )
            VALUES (
                $1, $2, $3, digest($4, 'sha256'),
                now() - interval '2 days',
                now() - interval '2 days',
                now() - interval '1 day'
            )
            "#,
        )
        .bind(expired_session_id)
        .bind(workspace_id)
        .bind(fan_id)
        .bind(format!("expired-session-{suffix}"))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO fan_action_tokens (
                id, workspace_id, fan_id, purpose, token_hash,
                created_at, expires_at
            )
            VALUES (
                $1, $2, $3, 'confirm', digest($4, 'sha256'),
                now() - interval '2 days',
                now() - interval '1 day'
            )
            "#,
        )
        .bind(expired_action_id)
        .bind(workspace_id)
        .bind(fan_id)
        .bind(format!("expired-action-{suffix}"))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO fan_action_tokens (
                id, workspace_id, fan_id, purpose, token_hash,
                created_at, expires_at, consumed_at
            )
            VALUES (
                $1, $2, $3, 'unsubscribe', digest($4, 'sha256'),
                now() - interval '1 day',
                now() + interval '30 days',
                now()
            )
            "#,
        )
        .bind(recent_consumed_action_id)
        .bind(workspace_id)
        .bind(fan_id)
        .bind(format!("recent-action-{suffix}"))
        .execute(&pool)
        .await?;

        sqlx::query(
            r#"
            INSERT INTO webhook_endpoints (
                id, workspace_id, name, url, signing_secret_ref
            )
            VALUES ($1, $2, 'Retention endpoint', 'https://example.test/hook', 'retention-secret')
            "#,
        )
        .bind(endpoint_id)
        .bind(workspace_id)
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO outbox_events (
                id, workspace_id, event_type, payload, status, delivered_at
            )
            VALUES ($1, $2, 'fan.confirmed', $3, 'delivered', now())
            "#,
        )
        .bind(scrubbed_event_id)
        .bind(workspace_id)
        .bind(json!({
            "email": "fan@example.test",
            "unsubscribe_token": "remove-me",
            "confirmation_token": "remove-me-too"
        }))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO outbox_events (
                id, workspace_id, event_type, payload, status,
                created_at, delivered_at
            )
            VALUES (
                $1, $2, 'admission.pass.issued', $3, 'delivered',
                now() - interval '32 days',
                now() - interval '31 days'
            )
            "#,
        )
        .bind(blocked_event_id)
        .bind(workspace_id)
        .bind(json!({"claim_token": "still-needed"}))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO webhook_deliveries (
                id, workspace_id, outbox_event_id, endpoint_id,
                status, max_attempts, created_at
            )
            VALUES (
                $1, $2, $3, $4, 'pending', 3,
                now() - interval '31 days'
            )
            "#,
        )
        .bind(blocked_delivery_id)
        .bind(workspace_id)
        .bind(blocked_event_id)
        .bind(endpoint_id)
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO outbox_events (
                id, workspace_id, event_type, payload, status,
                created_at, delivered_at
            )
            VALUES (
                $1, $2, 'merch_coupon.issued', $3, 'delivered',
                now() - interval '32 days',
                now() - interval '31 days'
            )
            "#,
        )
        .bind(deleted_event_id)
        .bind(workspace_id)
        .bind(json!({"coupon_code": "delete-with-parent"}))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO webhook_deliveries (
                id, workspace_id, outbox_event_id, endpoint_id,
                status, attempt_count, max_attempts,
                created_at, delivered_at
            )
            VALUES (
                $1, $2, $3, $4, 'delivered', 1, 3,
                now() - interval '32 days',
                now() - interval '31 days'
            )
            "#,
        )
        .bind(deleted_delivery_id)
        .bind(workspace_id)
        .bind(deleted_event_id)
        .bind(endpoint_id)
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO webhook_delivery_attempts (
                workspace_id, delivery_id, attempt_number,
                started_at, finished_at, outcome,
                response_status, duration_ms
            )
            VALUES (
                $1, $2, 1,
                now() - interval '31 days' - interval '1 second',
                now() - interval '31 days',
                'delivered', 204, 1000
            )
            "#,
        )
        .bind(workspace_id)
        .bind(deleted_delivery_id)
        .execute(&pool)
        .await?;

        let worker = RetentionWorker::new(
            pool.clone(),
            RetentionWorkerConfig {
                operation_timeout: Duration::from_secs(5),
                lock_timeout: Duration::from_secs(1),
                ..RetentionWorkerConfig::default()
            },
        )?;
        let stats = worker.run_once().await?;
        assert!(stats.idempotency_keys_deleted >= 1);
        assert!(stats.webhook_replay_keys_deleted >= 1);
        assert!(stats.fan_sessions_deleted >= 1);
        assert!(stats.fan_action_tokens_deleted >= 1);
        assert_eq!(stats.expired_admission_passes_reconciled, 1);
        assert!(stats.outbox_payloads_scrubbed >= 1);
        assert!(stats.terminal_outbox_events_deleted >= 1);

        assert!(
            !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                    SELECT 1 FROM idempotency_keys
                    WHERE workspace_id = $1 AND scope = 'retention-test' AND key = $2
                )",
            )
            .bind(workspace_id)
            .bind(&expired_idempotency_key)
            .fetch_one(&pool)
            .await?
        );
        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                    SELECT 1 FROM idempotency_keys
                    WHERE workspace_id = $1 AND scope = 'retention-test' AND key = $2
                )",
            )
            .bind(workspace_id)
            .bind(&retained_idempotency_key)
            .fetch_one(&pool)
            .await?
        );
        let (pass_status, claim_token_hash) = sqlx::query_as::<_, (String, Option<Vec<u8>>)>(
            "SELECT status, claim_token_hash FROM admission_passes WHERE id = $1",
        )
        .bind(expired_pass_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(pass_status, "expired");
        assert_eq!(claim_token_hash, None);
        assert_eq!(
            sqlx::query_scalar::<_, i32>("SELECT issued_count FROM admission_pools WHERE id = $1",)
                .bind(admission_pool_id)
                .fetch_one(&pool)
                .await?,
            0
        );
        assert!(
            !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                    SELECT 1 FROM webhook_replay_keys
                    WHERE workspace_id = $1 AND source = 'retention-test' AND event_id = $2
                )",
            )
            .bind(workspace_id)
            .bind(&expired_replay_id)
            .fetch_one(&pool)
            .await?
        );
        assert!(
            !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM fan_sessions WHERE id = $1)",
            )
            .bind(expired_session_id)
            .fetch_one(&pool)
            .await?
        );
        assert!(
            !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM fan_action_tokens WHERE id = $1)",
            )
            .bind(expired_action_id)
            .fetch_one(&pool)
            .await?
        );
        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM fan_action_tokens WHERE id = $1)",
            )
            .bind(recent_consumed_action_id)
            .fetch_one(&pool)
            .await?
        );

        let scrubbed_payload = sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT payload FROM outbox_events WHERE id = $1",
        )
        .bind(scrubbed_event_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            scrubbed_payload
                .get("email")
                .and_then(|value| value.as_str()),
            Some("fan@example.test")
        );
        assert!(scrubbed_payload.get("unsubscribe_token").is_none());
        assert!(scrubbed_payload.get("confirmation_token").is_none());

        let blocked_payload = sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT payload FROM outbox_events WHERE id = $1",
        )
        .bind(blocked_event_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            blocked_payload
                .get("claim_token")
                .and_then(|value| value.as_str()),
            Some("still-needed")
        );
        assert!(
            !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM outbox_events WHERE id = $1)",
            )
            .bind(deleted_event_id)
            .fetch_one(&pool)
            .await?
        );
        assert!(
            !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM webhook_deliveries WHERE id = $1)",
            )
            .bind(deleted_delivery_id)
            .fetch_one(&pool)
            .await?
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM webhook_delivery_attempts WHERE delivery_id = $1",
            )
            .bind(deleted_delivery_id)
            .fetch_one(&pool)
            .await?,
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM audit_events
                    WHERE workspace_id = $1 AND action = 'retention.test'",
            )
            .bind(workspace_id)
            .fetch_one(&pool)
            .await?,
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM fan_consents
                    WHERE workspace_id = $1 AND fan_id = $2",
            )
            .bind(workspace_id)
            .bind(fan_id)
            .fetch_one(&pool)
            .await?,
            1
        );

        let second_stats = worker.run_once().await?;
        assert_eq!(second_stats.expired_admission_passes_reconciled, 0);
        assert_eq!(
            sqlx::query_scalar::<_, i32>("SELECT issued_count FROM admission_pools WHERE id = $1",)
                .bind(admission_pool_id)
                .fetch_one(&pool)
                .await?,
            0,
            "reconciliation must be idempotent"
        );

        pool.close().await;
        Ok(())
    }

    #[test]
    fn stats_report_work_only_for_changed_rows() {
        assert!(!RetentionStats::default().did_work());
        assert!(
            RetentionStats {
                outbox_payloads_scrubbed: 1,
                ..RetentionStats::default()
            }
            .did_work()
        );
    }
}
