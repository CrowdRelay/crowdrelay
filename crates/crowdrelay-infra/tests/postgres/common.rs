//! Shared PostgreSQL test wiring: the whole suite runs against ONE migrated
//! database named by the environment variable.
//!
//! Each test gets a fresh pool on that one database — a TCP connect is a
//! millisecond, and a per-test pool keeps session state (temp tables,
//! advisory locks, a stray `pool.close()`) scoped to the test that made it.
//! The previous clone-per-test machinery paid CREATE DATABASE per test;
//! this pays a connect.

use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

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

/// A globally unique slug for a fixture row. Slugs on `organizations`,
/// `workspaces` and `cities` are unique across the whole table, and the suite
/// shares one database, so a fixed slug collides with the same test's earlier
/// run or a sibling's. Keep the readable label in `name`; the slug carries the
/// random tail of the row's UUIDv7.
pub fn unique_slug(label: &str, id: Uuid) -> String {
    format!("{label}-{}", &id.simple().to_string()[20..])
}

/// A database of the test's own, migrated from empty, for the few tests
/// whose subject is a global selection over a shared catalogue (no workspace
/// column) and whose counts only mean something when the table holds exactly
/// what the test put there. Emptying that table on the shared suite database
/// instead deletes every other test's fixtures and the migration-seeded rows
/// they depend on.
///
/// Costs one CREATE DATABASE and one migration run, so keep it for that case.
/// Call [`IsolatedDatabase::drop`] when done; a panicking test leaks the
/// database, which the per-run suite database's own teardown does not cover,
/// so its name carries the suite database's name as a prefix to find it.
pub struct IsolatedDatabase {
    pub pool: PgPool,
    admin: PgPool,
    name: String,
}

pub async fn isolated_database(env_var: &str) -> Result<IsolatedDatabase, sqlx::Error> {
    use sqlx::postgres::PgConnectOptions;
    use std::str::FromStr;

    let (admin, url) = test_pool_with_url(env_var).await?;
    let suite = PgConnectOptions::from_str(&url)?;
    let prefix = suite.get_database().unwrap_or("crowdrelay");
    let name = format!(
        "{prefix}_iso_{}",
        &Uuid::now_v7().simple().to_string()[20..]
    );
    sqlx::query(&format!("CREATE DATABASE \"{name}\""))
        .execute(&admin)
        .await?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(suite.database(&name))
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    Ok(IsolatedDatabase { pool, admin, name })
}

impl IsolatedDatabase {
    pub async fn drop(self) -> Result<(), sqlx::Error> {
        self.pool.close().await;
        sqlx::query(&format!(
            "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
            self.name
        ))
        .execute(&self.admin)
        .await?;
        Ok(())
    }
}

/// A peer row seeded directly, for suites whose subject is machinery
/// downstream of the peer registry rather than the registry itself.
///
/// Peer writes go through the operator-ledger paths now (`create_operator_peer`,
/// `resolve_peer_operator`); the unguarded `create_peer`/`resolve_peer` they
/// used to share were removed as unwired, so a fixture that just needs a
/// standing row writes it the way a proposal leaves it.
pub async fn seed_peer(
    pool: &PgPool,
    workspace_id: Uuid,
    name: &str,
    status: &str,
    rejection_reason: Option<&str>,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO peers (
             id, workspace_id, name, handles, tier, watch_for, why,
             proposed_by, status, rejection_reason, confirmed_at
         )
         VALUES ($1, $2, $3, '{}'::jsonb, 'near_peer', '{}', 'fixture', 'fixture',
                 $4, $5, CASE WHEN $4 = 'confirmed' THEN now() END)
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id)
    .bind(name)
    .bind(status)
    .bind(rejection_reason)
    .fetch_one(pool)
    .await
}
