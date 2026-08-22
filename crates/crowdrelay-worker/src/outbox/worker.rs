use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use sqlx::PgPool;
use thiserror::Error;
use time::OffsetDateTime;
use tokio::{
    sync::watch,
    task::JoinSet,
    time::{sleep, timeout},
};
use uuid::Uuid;

use super::{
    SecretProvider, SecretProviderErrorKind,
    backoff::retry_delay,
    model::{AttemptOutcome, AttemptResolution, DeliveryClaim, OutboxEventClaim},
    repository::{EligibilityTarget, PgOutboxStore, StoreError, eligibility_target},
    transport::{DispatchDisposition, DispatchResult, TransportBuildError, WebhookDispatcher},
};

const MAX_ENDPOINT_TIMEOUT: Duration = Duration::from_secs(60);

/// Per-delivery recipient eligibility resolved once per cycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EligibilityDecision {
    Eligible,
    Ineligible,
    /// The eligibility read failed; the delivery must retry with this kind.
    Unavailable(&'static str),
}

/// Transactional outbox worker that materializes and delivers webhook events.
#[derive(Clone)]
pub struct OutboxWorker {
    store: PgOutboxStore,
    secret_provider: Arc<dyn SecretProvider>,
    dispatcher: WebhookDispatcher,
    config: Arc<OutboxWorkerConfig>,
}

impl OutboxWorker {
    pub fn new(
        pool: PgPool,
        secret_provider: Arc<dyn SecretProvider>,
        config: OutboxWorkerConfig,
    ) -> Result<Self, WorkerBuildError> {
        config.validate()?;
        let dispatcher = WebhookDispatcher::new(
            config.http_connect_timeout,
            &config.http_user_agent,
            config.allow_http_endpoints,
        )
        .map_err(|_error: TransportBuildError| WorkerBuildError::HttpClient)?;

        Ok(Self {
            store: PgOutboxStore::new(pool),
            secret_provider,
            dispatcher,
            config: Arc::new(config),
        })
    }

    /// Runs until the watch value becomes `true` or all senders are dropped.
    ///
    /// Database outages are treated as an operational condition: the worker
    /// logs only a sanitized failure kind, waits for the bounded polling
    /// interval, and tries again. Individual webhook retries remain bounded by
    /// the endpoint's persisted `max_attempts`.
    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) {
        loop {
            if shutdown_requested(&shutdown) {
                return;
            }

            match self.run_cycle(&mut shutdown).await {
                Ok(stats) => {
                    if stats.did_work() {
                        tracing::info!(
                            outbox_claimed = stats.outbox_claimed,
                            deliveries_materialized = stats.deliveries_materialized,
                            deliveries_claimed = stats.deliveries_claimed,
                            delivered = stats.delivered,
                            retried = stats.retried,
                            dead = stats.dead,
                            "outbox cycle completed"
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        operation = error.operation,
                        error_kind = error.kind,
                        "outbox cycle could not complete"
                    );
                }
            }

            if wait_or_shutdown(&mut shutdown, self.config.poll_interval).await {
                return;
            }
        }
    }

    /// Executes one bounded cycle, primarily for health probes and integration
    /// tests. It does not wait for the regular poll interval.
    pub async fn run_once(&self) -> Result<RunStats, WorkerRunError> {
        let (_shutdown_tx, mut shutdown_rx) = watch::channel(false);
        self.run_cycle(&mut shutdown_rx).await
    }

    async fn run_cycle(
        &self,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<RunStats, WorkerRunError> {
        let mut stats = RunStats::default();

        let outbox_claims = self
            .database_call(
                "claim_outbox",
                self.store.claim_outbox_events(
                    &self.config.worker_id,
                    self.config.outbox_batch_size,
                    self.config.lease_duration,
                ),
            )
            .await?;
        stats.outbox_claimed = outbox_claims.len() as u64;

        if !outbox_claims.is_empty() {
            match self
                .database_call(
                    "materialize_webhook_deliveries_batch",
                    self.store
                        .materialize_deliveries_batch(&outbox_claims, &self.config.worker_id),
                )
                .await
            {
                Ok(created) => {
                    stats.deliveries_materialized =
                        stats.deliveries_materialized.saturating_add(created);
                }
                Err(error) => {
                    tracing::warn!(
                        claimed = outbox_claims.len(),
                        operation = error.operation,
                        error_kind = error.kind,
                        "failed to materialize outbox batch"
                    );
                    for claim in &outbox_claims {
                        self.release_failed_outbox(claim, error.kind).await;
                    }
                }
            }
        }

        if shutdown_requested(shutdown) {
            return Ok(stats);
        }

        let delivery_claims = self
            .database_call(
                "claim_webhook_deliveries",
                self.store.claim_deliveries(
                    &self.config.worker_id,
                    i64::try_from(self.config.max_concurrent_deliveries).unwrap_or(i64::MAX),
                    self.config.lease_duration,
                ),
            )
            .await?;
        stats.deliveries_claimed = delivery_claims.len() as u64;

        let eligibility = self.resolve_eligibility(&delivery_claims).await;

        let mut tasks = JoinSet::new();
        for (claim, decision) in delivery_claims.into_iter().zip(eligibility) {
            let worker = self.clone();
            let task_shutdown = shutdown.clone();
            tasks.spawn(async move { worker.dispatch_one(claim, decision, task_shutdown).await });
        }

        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(outcome)) => stats.record(outcome),
                Ok(Err(error)) => {
                    tracing::warn!(
                        operation = error.operation,
                        error_kind = error.kind,
                        "webhook attempt could not be persisted"
                    );
                }
                Err(join_error) => {
                    tracing::error!(
                        cancelled = join_error.is_cancelled(),
                        panic = join_error.is_panic(),
                        "webhook task stopped unexpectedly; its lease will be recovered"
                    );
                }
            }
        }

        Ok(stats)
    }

    async fn release_failed_outbox(&self, claim: &OutboxEventClaim, failure_kind: &'static str) {
        let error_kind = if failure_kind == "timeout" {
            "materialization_timeout"
        } else {
            "materialization_database"
        };
        let delay = retry_delay(
            self.config.backoff_base,
            self.config.backoff_cap,
            claim.attempt_number,
            claim.id,
        );

        if let Err(error) = self
            .database_call(
                "release_outbox",
                self.store.fail_outbox_event(
                    claim,
                    &self.config.worker_id,
                    true,
                    delay,
                    error_kind,
                ),
            )
            .await
        {
            tracing::warn!(
                event_id = %claim.id,
                workspace_id = %claim.workspace_id,
                operation = error.operation,
                error_kind = error.kind,
                "failed to release outbox event; its lease will be recovered"
            );
        }
    }

    /// Resolves recipient eligibility for the whole claimed batch with a single
    /// database read, keeping the per-delivery decisions the dispatch path used
    /// to make one query at a time.
    ///
    /// A failed lookup only marks the deliveries that actually gate on a fan as
    /// retryable; ungated deliveries still dispatch, exactly as when each
    /// delivery resolved its own eligibility.
    async fn resolve_eligibility(&self, claims: &[DeliveryClaim]) -> Vec<EligibilityDecision> {
        let targets: Vec<EligibilityTarget> = claims
            .iter()
            .map(|claim| eligibility_target(&claim.event_type, &claim.payload))
            .collect();
        let recipients: Vec<(Uuid, Uuid)> = claims
            .iter()
            .zip(&targets)
            .filter_map(|(claim, target)| match *target {
                EligibilityTarget::Fan { fan_id, .. } => Some((claim.workspace_id, fan_id)),
                EligibilityTarget::NotGated | EligibilityTarget::MissingRecipient => None,
            })
            .collect();

        let consent = if recipients.is_empty() {
            Ok(std::collections::HashMap::new())
        } else {
            self.database_call(
                "check_delivery_eligibility",
                self.store.active_fan_marketing_consent(&recipients),
            )
            .await
        };

        let unavailable_kind = consent.as_ref().err().map(|error| {
            if error.kind == "timeout" {
                "eligibility_timeout"
            } else {
                "eligibility_database"
            }
        });

        claims
            .iter()
            .zip(&targets)
            .map(|(claim, target)| match *target {
                EligibilityTarget::NotGated => EligibilityDecision::Eligible,
                EligibilityTarget::MissingRecipient => EligibilityDecision::Ineligible,
                EligibilityTarget::Fan {
                    fan_id,
                    require_consent,
                } => match (&consent, unavailable_kind) {
                    (Ok(consent), _) => {
                        if consent
                            .get(&(claim.workspace_id, fan_id))
                            .is_some_and(|granted| *granted || !require_consent)
                        {
                            EligibilityDecision::Eligible
                        } else {
                            EligibilityDecision::Ineligible
                        }
                    }
                    (Err(_), Some(kind)) => EligibilityDecision::Unavailable(kind),
                    (Err(_), None) => EligibilityDecision::Unavailable("eligibility_database"),
                },
            })
            .collect()
    }

    async fn dispatch_one(
        &self,
        claim: DeliveryClaim,
        eligibility: EligibilityDecision,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<AttemptOutcome, WorkerRunError> {
        let started_at = OffsetDateTime::now_utc();
        let started = Instant::now();
        let result = self
            .resolve_and_dispatch(&claim, eligibility, &mut shutdown)
            .await;
        let outcome = final_outcome(result.disposition, claim.attempt_number, claim.max_attempts);
        let retry_delay = if outcome == AttemptOutcome::Retry {
            retry_delay(
                self.config.backoff_base,
                self.config.backoff_cap,
                claim.attempt_number,
                claim.delivery_id,
            )
        } else {
            Duration::ZERO
        };
        let finished_at = OffsetDateTime::now_utc().max(started_at);
        let resolution = AttemptResolution {
            outcome,
            response_status: result.response_status,
            error_kind: result.error_kind,
            retry_delay_ms: i64::try_from(retry_delay.as_millis()).unwrap_or(i64::MAX),
            started_at,
            finished_at,
            duration_ms: i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX),
        };

        self.database_call(
            "finish_webhook_delivery",
            self.store
                .finish_delivery(&claim, &self.config.worker_id, &resolution),
        )
        .await?;

        tracing::info!(
            delivery_id = %claim.delivery_id,
            event_id = %claim.event_id,
            endpoint_id = %claim.endpoint_id,
            workspace_id = %claim.workspace_id,
            attempt = claim.attempt_number,
            outcome = outcome.as_str(),
            response_status = result.response_status,
            error_kind = result.error_kind,
            "webhook attempt finished"
        );

        Ok(outcome)
    }

    async fn resolve_and_dispatch(
        &self,
        claim: &DeliveryClaim,
        eligibility: EligibilityDecision,
        shutdown: &mut watch::Receiver<bool>,
    ) -> DispatchResult {
        if shutdown_requested(shutdown) {
            return DispatchResult::retryable(None, "worker_shutdown");
        }

        match eligibility {
            EligibilityDecision::Eligible => {}
            EligibilityDecision::Ineligible => {
                return DispatchResult::permanent("recipient_ineligible");
            }
            EligibilityDecision::Unavailable(error_kind) => {
                return DispatchResult::retryable(None, error_kind);
            }
        }

        let resolve_secret = timeout(
            self.config.secret_resolution_timeout,
            self.secret_provider.resolve(&claim.signing_secret_ref),
        );
        let secret = tokio::select! {
            biased;
            () = shutdown_signal(shutdown) => {
                return DispatchResult::retryable(None, "worker_shutdown");
            }
            result = resolve_secret => {
                match result {
                    Ok(Ok(secret)) => secret,
                    Ok(Err(error)) => {
                        return match error.kind() {
                            SecretProviderErrorKind::Unavailable => {
                                DispatchResult::retryable(None, "secret_unavailable")
                            }
                            SecretProviderErrorKind::NotFound => {
                                DispatchResult::permanent("secret_not_found")
                            }
                            SecretProviderErrorKind::Invalid => {
                                DispatchResult::permanent("secret_invalid")
                            }
                        };
                    }
                    Err(_) => return DispatchResult::retryable(None, "secret_timeout"),
                }
            }
        };

        tokio::select! {
            biased;
            () = shutdown_signal(shutdown) => {
                DispatchResult::retryable(None, "worker_shutdown")
            }
            result = self.dispatcher.dispatch(claim, &secret) => result,
        }
    }

    async fn database_call<T>(
        &self,
        operation: &'static str,
        future: impl Future<Output = Result<T, StoreError>>,
    ) -> Result<T, WorkerRunError> {
        match timeout(self.config.database_operation_timeout, future).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(WorkerRunError {
                operation,
                kind: error.kind(),
            }),
            Err(_) => Err(WorkerRunError {
                operation,
                kind: "timeout",
            }),
        }
    }
}

fn final_outcome(
    disposition: DispatchDisposition,
    attempt_number: i32,
    max_attempts: i32,
) -> AttemptOutcome {
    match disposition {
        DispatchDisposition::Delivered => AttemptOutcome::Delivered,
        DispatchDisposition::Retryable if attempt_number < max_attempts => AttemptOutcome::Retry,
        DispatchDisposition::Retryable | DispatchDisposition::Permanent => AttemptOutcome::Dead,
    }
}

fn shutdown_requested(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow()
}

async fn shutdown_signal(shutdown: &mut watch::Receiver<bool>) {
    if shutdown_requested(shutdown) {
        return;
    }

    loop {
        if shutdown.changed().await.is_err() || shutdown_requested(shutdown) {
            return;
        }
    }
}

async fn wait_or_shutdown(shutdown: &mut watch::Receiver<bool>, duration: Duration) -> bool {
    if shutdown_requested(shutdown) {
        return true;
    }

    tokio::select! {
        () = shutdown_signal(shutdown) => true,
        () = sleep(duration) => false,
    }
}

/// Validated runtime configuration for the outbox worker.
#[derive(Clone, Debug)]
pub struct OutboxWorkerConfig {
    pub worker_id: String,
    pub poll_interval: Duration,
    pub outbox_batch_size: i64,
    pub max_concurrent_deliveries: usize,
    pub lease_duration: Duration,
    pub database_operation_timeout: Duration,
    pub secret_resolution_timeout: Duration,
    pub http_connect_timeout: Duration,
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    pub allow_http_endpoints: bool,
    pub http_user_agent: String,
}

impl Default for OutboxWorkerConfig {
    fn default() -> Self {
        Self {
            worker_id: format!("outbox-{}", Uuid::now_v7().simple()),
            poll_interval: Duration::from_millis(250),
            outbox_batch_size: 100,
            max_concurrent_deliveries: 8,
            lease_duration: Duration::from_secs(180),
            database_operation_timeout: Duration::from_secs(10),
            secret_resolution_timeout: Duration::from_secs(5),
            http_connect_timeout: Duration::from_secs(5),
            backoff_base: Duration::from_secs(1),
            backoff_cap: Duration::from_secs(15 * 60),
            allow_http_endpoints: false,
            http_user_agent: format!("crowdrelay-worker/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

impl OutboxWorkerConfig {
    pub fn validate(&self) -> Result<(), OutboxWorkerConfigError> {
        if self.worker_id.trim().is_empty()
            || self.worker_id.len() > 128
            || !self
                .worker_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(OutboxWorkerConfigError::WorkerId);
        }
        if !(1..=1_000).contains(&self.outbox_batch_size) {
            return Err(OutboxWorkerConfigError::OutboxBatchSize);
        }
        if !(1..=256).contains(&self.max_concurrent_deliveries) {
            return Err(OutboxWorkerConfigError::Concurrency);
        }
        if !(Duration::from_millis(10)..=Duration::from_secs(60)).contains(&self.poll_interval) {
            return Err(OutboxWorkerConfigError::PollInterval);
        }
        if !(Duration::from_millis(100)..=Duration::from_secs(60))
            .contains(&self.database_operation_timeout)
        {
            return Err(OutboxWorkerConfigError::DatabaseTimeout);
        }
        if !(Duration::from_millis(100)..=Duration::from_secs(60))
            .contains(&self.secret_resolution_timeout)
        {
            return Err(OutboxWorkerConfigError::SecretTimeout);
        }
        if !(Duration::from_millis(100)..=Duration::from_secs(60))
            .contains(&self.http_connect_timeout)
        {
            return Err(OutboxWorkerConfigError::ConnectTimeout);
        }
        let required_lease = self
            .database_operation_timeout
            .saturating_add(self.secret_resolution_timeout)
            .saturating_add(MAX_ENDPOINT_TIMEOUT);
        if self.lease_duration < required_lease
            || self.lease_duration > Duration::from_secs(60 * 60)
        {
            return Err(OutboxWorkerConfigError::LeaseDuration {
                minimum: required_lease,
            });
        }
        if self.backoff_base < Duration::from_millis(100)
            || self.backoff_cap < self.backoff_base
            || self.backoff_cap > Duration::from_secs(24 * 60 * 60)
        {
            return Err(OutboxWorkerConfigError::Backoff);
        }
        if self.http_user_agent.trim().is_empty() || self.http_user_agent.len() > 255 {
            return Err(OutboxWorkerConfigError::UserAgent);
        }

        Ok(())
    }
}

/// Error returned when outbox worker configuration is invalid.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum OutboxWorkerConfigError {
    #[error("worker_id must be 1-128 ASCII letters, digits, dots, underscores, or hyphens")]
    WorkerId,

    #[error("outbox_batch_size must be between 1 and 1000")]
    OutboxBatchSize,

    #[error("max_concurrent_deliveries must be between 1 and 256")]
    Concurrency,

    #[error("poll_interval must be between 10 milliseconds and 60 seconds")]
    PollInterval,

    #[error("database_operation_timeout must be between 100 milliseconds and 60 seconds")]
    DatabaseTimeout,

    #[error("secret_resolution_timeout must be between 100 milliseconds and 60 seconds")]
    SecretTimeout,

    #[error("http_connect_timeout must be between 100 milliseconds and 60 seconds")]
    ConnectTimeout,

    #[error("lease_duration must be at least {minimum:?} and no longer than one hour")]
    LeaseDuration { minimum: Duration },

    #[error("backoff must start at 100 milliseconds, grow monotonically, and cap within one day")]
    Backoff,

    #[error("http_user_agent must contain between 1 and 255 characters")]
    UserAgent,
}

/// Error returned when the outbox worker fails to build from its configuration.
#[derive(Debug, Error)]
pub enum WorkerBuildError {
    #[error(transparent)]
    Config(#[from] OutboxWorkerConfigError),

    #[error("failed to build the bounded webhook HTTP client")]
    HttpClient,
}

/// Sanitized runtime error from a failed outbox worker operation.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("worker operation {operation} failed ({kind})")]
pub struct WorkerRunError {
    operation: &'static str,
    kind: &'static str,
}

impl WorkerRunError {
    pub const fn operation(&self) -> &'static str {
        self.operation
    }

    pub const fn kind(&self) -> &'static str {
        self.kind
    }
}

/// Cumulative counters from a single outbox worker run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunStats {
    pub outbox_claimed: u64,
    pub deliveries_materialized: u64,
    pub deliveries_claimed: u64,
    pub delivered: u64,
    pub retried: u64,
    pub dead: u64,
}

impl RunStats {
    pub const fn did_work(self) -> bool {
        self.outbox_claimed > 0 || self.deliveries_claimed > 0
    }

    fn record(&mut self, outcome: AttemptOutcome) {
        match outcome {
            AttemptOutcome::Delivered => self.delivered = self.delivered.saturating_add(1),
            AttemptOutcome::Retry => self.retried = self.retried.saturating_add(1),
            AttemptOutcome::Dead => self.dead = self.dead.saturating_add(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_configuration_is_valid() -> Result<(), Box<dyn std::error::Error>> {
        OutboxWorkerConfig::default().validate()?;
        Ok(())
    }

    #[test]
    fn default_lease_has_headroom_for_maximum_database_timeout()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = OutboxWorkerConfig {
            database_operation_timeout: Duration::from_secs(60),
            ..OutboxWorkerConfig::default()
        };
        config.validate()?;
        Ok(())
    }

    #[test]
    fn retry_exhaustion_transitions_to_dead() {
        assert_eq!(
            final_outcome(DispatchDisposition::Retryable, 2, 3),
            AttemptOutcome::Retry
        );
        assert_eq!(
            final_outcome(DispatchDisposition::Retryable, 3, 3),
            AttemptOutcome::Dead
        );
        assert_eq!(
            final_outcome(DispatchDisposition::Permanent, 1, 12),
            AttemptOutcome::Dead
        );
    }

    #[test]
    fn lease_must_cover_every_bounded_external_operation() {
        let config = OutboxWorkerConfig {
            lease_duration: Duration::from_secs(60),
            ..OutboxWorkerConfig::default()
        };

        assert!(matches!(
            config.validate(),
            Err(OutboxWorkerConfigError::LeaseDuration { .. })
        ));
    }
}
