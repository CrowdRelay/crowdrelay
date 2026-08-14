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

include!("retention/steps.rs");

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

include!("retention/tests.rs");
