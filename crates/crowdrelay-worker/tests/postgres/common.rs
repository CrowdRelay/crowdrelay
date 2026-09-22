//! Shared PostgreSQL test wiring: the whole suite runs against ONE migrated
//! database named by the environment variable.
//!
//! Each test gets a fresh pool on that one database — a TCP connect is a
//! millisecond, and a per-test pool keeps session state (temp tables,
//! advisory locks, a stray `pool.close()`) scoped to the test that made it.
//! The previous clone-per-test machinery paid CREATE DATABASE per test;
//! this pays a connect.

use sqlx::{PgPool, postgres::PgPoolOptions};

/// A fresh pool on the shared suite database named by `env_var`.
///
/// The migrate call re-checks the applied set — a no-op when the suite
/// database is current, and the guard against a stale or unmigrated one.
pub async fn test_pool(env_var: &str) -> Result<PgPool, sqlx::Error> {
    Ok(test_pool_with_url(env_var).await?.0)
}

/// Same pool plus the suite URL for tests whose repository builds a second
/// connection from a `DatabaseConfig` — it lands on the same database.
pub async fn test_pool_with_url(env_var: &str) -> Result<(PgPool, String), sqlx::Error> {
    let url = std::env::var(env_var).map_err(|e| {
        sqlx::Error::Configuration(
            format!("{env_var} must target the migrated suite database: {e}").into(),
        )
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    Ok((pool, url))
}
