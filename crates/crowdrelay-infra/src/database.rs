//! PostgreSQL pool lifecycle, readiness and migrations.
//!
//! Provides connection pool creation, readiness checks, and embedded
//! migrations shared by the API and worker binaries.

use std::time::Duration;

use sqlx::{
    PgPool,
    migrate::{MigrateError, Migrator},
    postgres::PgPoolOptions,
};
use thiserror::Error;
use tokio::time::timeout;

use crate::config::DatabaseConfig;

/// Migrations embedded into both binaries at compile time.
pub static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");

/// Stable application-facing classification of SQLx failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SqlxErrorClass {
    NotFound,
    Conflict,
    Unavailable,
    Unexpected,
}

/// Classifies PostgreSQL and pool failures consistently across repositories.
///
/// Only constraint failures that can reasonably be caused by a conflicting
/// request are mapped to `Conflict`. Schema/decoding errors remain unexpected
/// so deployment defects are not hidden behind a retryable 503.
#[must_use]
pub(crate) fn classify_sqlx_error(error: &sqlx::Error) -> SqlxErrorClass {
    let class = match error {
        sqlx::Error::RowNotFound => SqlxErrorClass::NotFound,
        sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::WorkerCrashed => SqlxErrorClass::Unavailable,
        sqlx::Error::Database(database) => {
            let code = database.code();
            let code = code.as_deref().unwrap_or_default();
            if code.starts_with("08")
                || matches!(
                    code,
                    "40001" | "40P01" | "53300" | "55P03" | "57014" | "57P01" | "57P02" | "57P03"
                )
            {
                SqlxErrorClass::Unavailable
            } else if code == "23505" {
                SqlxErrorClass::Conflict
            } else {
                SqlxErrorClass::Unexpected
            }
        }
        _ => SqlxErrorClass::Unexpected,
    };
    if class == SqlxErrorClass::Unexpected {
        if let sqlx::Error::Database(database) = error {
            tracing::error!(
                sqlstate = database.code().as_deref(),
                constraint = database.constraint(),
                "unexpected PostgreSQL persistence failure"
            );
        } else {
            tracing::error!("unexpected SQLx persistence failure");
        }
    }
    class
}

/// Builds a bounded pool without establishing a connection.
///
/// URL syntax is still checked synchronously. This is useful when the process
/// must finish assembling dependencies before deciding when to contact the
/// database.
pub fn connect_lazy(config: &DatabaseConfig) -> Result<PgPool, DatabaseError> {
    pool_options(config)
        .connect_lazy(&config.url)
        .map_err(DatabaseError::ConfigurePool)
}

/// Builds a bounded pool and establishes its initial PostgreSQL connection.
pub async fn connect(config: &DatabaseConfig) -> Result<PgPool, DatabaseError> {
    timeout(
        config.connect_timeout,
        pool_options(config).connect(&config.url),
    )
    .await
    .map_err(|_| DatabaseError::ConnectTimeout {
        timeout: config.connect_timeout,
    })?
    .map_err(DatabaseError::Connect)
}

/// Checks database readiness within the caller-provided deadline.
pub async fn ping(pool: &PgPool, deadline: Duration) -> Result<(), DatabaseError> {
    timeout(
        deadline,
        sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(pool),
    )
    .await
    .map_err(|_| DatabaseError::PingTimeout { timeout: deadline })?
    .map(|_| ())
    .map_err(DatabaseError::Ping)
}

/// Applies every pending embedded migration.
pub async fn migrate(pool: &PgPool) -> Result<(), DatabaseError> {
    MIGRATOR.run(pool).await.map_err(DatabaseError::Migration)
}

fn pool_options(config: &DatabaseConfig) -> PgPoolOptions {
    PgPoolOptions::new()
        .min_connections(0)
        .max_connections(config.max_connections)
        .acquire_timeout(config.connect_timeout)
}

/// Error returned when database pool creation, readiness, or migration fails.
#[derive(Debug, Error)]
pub enum DatabaseError {
    /// Pool configuration failed (e.g. invalid URL).
    #[error("failed to configure PostgreSQL pool")]
    ConfigurePool(#[source] sqlx::Error),

    /// The initial connection attempt timed out.
    #[error("PostgreSQL connection timed out after {timeout:?}")]
    ConnectTimeout { timeout: Duration },

    /// The initial connection attempt failed.
    #[error("failed to connect to PostgreSQL")]
    Connect(#[source] sqlx::Error),

    /// The readiness ping timed out.
    #[error("PostgreSQL readiness check timed out after {timeout:?}")]
    PingTimeout { timeout: Duration },

    /// The readiness ping failed.
    #[error("PostgreSQL readiness check failed")]
    Ping(#[source] sqlx::Error),

    /// A migration failed.
    #[error("PostgreSQL migration failed")]
    Migration(#[source] MigrateError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database_config(url: &str) -> DatabaseConfig {
        DatabaseConfig {
            url: url.to_owned(),
            max_connections: 7,
            connect_timeout: Duration::from_secs(1),
            ping_timeout: Duration::from_millis(50),
            operation_timeout: Duration::from_secs(5),
            lock_timeout: Duration::from_secs(1),
        }
    }

    #[tokio::test]
    async fn lazy_pool_is_bounded_by_configuration() -> Result<(), Box<dyn std::error::Error>> {
        let pool = connect_lazy(&database_config(
            "postgres://crowdrelay:secret@localhost/crowdrelay",
        ))?;

        assert_eq!(pool.options().get_max_connections(), 7);
        pool.close().await;
        Ok(())
    }

    #[test]
    fn lazy_pool_rejects_invalid_url() {
        let result = connect_lazy(&database_config("not a PostgreSQL URL"));
        assert!(matches!(result, Err(DatabaseError::ConfigurePool(_))));
    }

    #[test]
    fn classifies_pool_and_lookup_failures() {
        assert_eq!(
            classify_sqlx_error(&sqlx::Error::PoolTimedOut),
            SqlxErrorClass::Unavailable
        );
        assert_eq!(
            classify_sqlx_error(&sqlx::Error::RowNotFound),
            SqlxErrorClass::NotFound
        );
    }
}
