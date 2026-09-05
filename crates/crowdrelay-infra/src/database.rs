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

/// PostgreSQL 18 is a production/runtime contract, not merely a CI fixture.
pub const MIN_POSTGRES_SERVER_VERSION_NUM: i32 = 180_000;

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
                message = %database.message(),
                "unexpected PostgreSQL persistence failure"
            );
        } else {
            tracing::error!(error = %error, "unexpected SQLx persistence failure");
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
    let pool = timeout(
        config.connect_timeout,
        pool_options(config).connect(&config.url),
    )
    .await
    .map_err(|_| DatabaseError::ConnectTimeout {
        timeout: config.connect_timeout,
    })?
    .map_err(DatabaseError::Connect)?;

    let version = timeout(
        config.connect_timeout,
        sqlx::query_scalar::<_, i32>("SELECT current_setting('server_version_num')::integer")
            .fetch_one(&pool),
    )
    .await
    .map_err(|_| DatabaseError::VersionCheckTimeout {
        timeout: config.connect_timeout,
    })?
    .map_err(DatabaseError::VersionCheck)?;

    if version < MIN_POSTGRES_SERVER_VERSION_NUM {
        pool.close().await;
        return Err(DatabaseError::UnsupportedServerVersion {
            actual: version,
            minimum: MIN_POSTGRES_SERVER_VERSION_NUM,
        });
    }

    report_schema_skew(&pool, config.connect_timeout).await;

    Ok(pool)
}

/// Says so when the database has migrations this build does not contain.
///
/// The two numbers can disagree, and when they do nothing noticed. Production
/// ran a build whose newest migration was 0234 while the database had 0236
/// applied: `/v1/meta` reported 234, because that constant is baked in at build
/// time from the migrations directory and describes the binary rather than the
/// database. The gap was invisible from every surface.
///
/// It is not harmless. 0236 widened a CHECK and demoted one row's `status` to a
/// value the running code had never heard of, and that code filters on
/// `status = 'connected'` — so a connection silently dropped out of the metric
/// sync, with no error anywhere, because the schema had moved and the code had
/// not.
///
/// A warning rather than a refusal. Migrations are applied ahead of a cutover on
/// purpose during blue-green: the green containers migrate, and for the length
/// of the soak the blue ones are legitimately behind. Refusing to start would
/// turn the normal case into an outage. What was missing is that anybody could
/// tell the normal case from a deploy that stopped halfway.
async fn report_schema_skew(pool: &PgPool, deadline: Duration) {
    let applied = timeout(
        deadline,
        sqlx::query_scalar::<_, Option<i64>>("SELECT max(version) FROM _sqlx_migrations")
            .fetch_one(pool),
    )
    .await;
    let Ok(Ok(Some(applied))) = applied else {
        // A database with no migration table has not been set up yet, and a
        // failed read here must never keep a process from starting: this
        // reports on the deploy, it does not gate it.
        return;
    };
    let built = MIGRATOR
        .migrations
        .iter()
        .map(|migration| migration.version)
        .max()
        .unwrap_or(0);
    if applied > built {
        tracing::warn!(
            database_schema_version = applied,
            build_schema_version = built,
            "the database has migrations this build does not contain: it was migrated by a newer \
             release. During a blue-green soak this is expected and clears at cutover. Outside one \
             it means a deploy applied its migrations and never finished, and this process is \
             reading a schema it was not written against."
        );
    }
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
    // Apply the query deadlines once per connection instead of relying solely on
    // the transaction-local `set_config` each write path issues.
    //
    // Two reasons. Statements executed straight against the pool never open a
    // transaction, so until now they ran with no statement_timeout at all: a
    // single slow read could occupy a connection indefinitely. And a session
    // default makes the per-transaction round trip redundant rather than
    // load-bearing, so it can be retired without changing behaviour.
    //
    // The values are constants from configuration, so a session default and a
    // transaction-local override resolve to the same deadline; the transaction
    // helpers remain correct while they exist.
    let statement_ms = config.operation_timeout.as_millis();
    let lock_ms = config.lock_timeout.as_millis();
    PgPoolOptions::new()
        .min_connections(1)
        .max_connections(config.max_connections)
        .acquire_timeout(config.connect_timeout)
        .idle_timeout(Some(Duration::from_secs(5 * 60)))
        .max_lifetime(Some(Duration::from_secs(30 * 60)))
        .after_connect(move |connection, _meta| {
            Box::pin(async move {
                sqlx::query(
                    "SELECT set_config('statement_timeout', $1, false), \
                     set_config('lock_timeout', $2, false)",
                )
                .bind(format!("{statement_ms}ms"))
                .bind(format!("{lock_ms}ms"))
                .execute(connection)
                .await?;
                Ok(())
            })
        })
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

    /// The server version preflight timed out.
    #[error("PostgreSQL server-version check timed out after {timeout:?}")]
    VersionCheckTimeout { timeout: Duration },

    /// The server version query failed.
    #[error("failed to determine PostgreSQL server version")]
    VersionCheck(#[source] sqlx::Error),

    /// CrowdRelay requires PostgreSQL 18+ for its production runtime contract.
    #[error(
        "unsupported PostgreSQL server version {actual}; require server_version_num >= {minimum}"
    )]
    UnsupportedServerVersion { actual: i32, minimum: i32 },

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
    fn postgres_18_is_a_runtime_contract() {
        assert_eq!(MIN_POSTGRES_SERVER_VERSION_NUM, 180_000);
        let error = DatabaseError::UnsupportedServerVersion {
            actual: 160_014,
            minimum: MIN_POSTGRES_SERVER_VERSION_NUM,
        };
        assert!(error.to_string().contains("180000"));
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

    /// The embedded migrator agrees with the migrations directory.
    ///
    /// This does NOT catch a stale `sqlx::migrate!` — adding a migration file
    /// does not force a rebuild, and a binary stale enough to miss one is stale
    /// enough that this test misses it too. That failure is real and was hit
    /// while building the skew check above; `touch`ing this file is the cure.
    ///
    /// What it does catch is a migration the migrator refuses to see at all: a
    /// filename the version parser rejects, or an empty embed. The skew warning
    /// reads `MIGRATOR.migrations` to decide what this build contains, so an
    /// empty or truncated list would make it compare a real database version
    /// against nothing and stay silent for ever.
    #[test]
    fn the_migrator_knows_its_newest_migration() {
        let built = MIGRATOR
            .migrations
            .iter()
            .map(|migration| migration.version)
            .max()
            .expect("the embedded migrator must carry migrations");
        let newest_file =
            std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../migrations"))
                .expect("migrations directory")
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    name.split('_').next()?.parse::<i64>().ok()
                })
                .max()
                .expect("at least one migration on disk");
        assert_eq!(
            built, newest_file,
            "the embedded migrator and the migrations directory disagree in this build: a \
             migration file is present that the migrator did not parse, most likely a filename \
             the version prefix parser rejects"
        );
    }
}
