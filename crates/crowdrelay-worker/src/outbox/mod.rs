//! Transactional outbox materialization and signed webhook delivery.
//!
//! The module deliberately does not read process environment variables.
//! Callers supply validated runtime policy and a [`SecretProvider`].

mod backoff;
mod model;
mod repository;
mod secrets;
mod signature;
mod transport;
mod worker;

pub use secrets::{
    MapSecretProvider, SecretProvider, SecretProviderError, SecretProviderErrorKind, SecretValue,
    SecretValueError,
};
pub use signature::{WebhookSignatureError, sign_webhook};
pub use transport::{
    CROWDRELAY_EVENT_ID, CROWDRELAY_EVENT_TYPE, CROWDRELAY_EVENT_VERSION, CROWDRELAY_SIGNATURE,
    CROWDRELAY_TIMESTAMP,
};
pub use worker::{
    OutboxWorker, OutboxWorkerConfig, OutboxWorkerConfigError, RunStats, WorkerBuildError,
    WorkerRunError,
};
