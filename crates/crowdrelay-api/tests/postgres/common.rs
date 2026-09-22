//! Shared PostgreSQL test wiring: the suite database is a template, each
//! test clones it.
//!
//! The CI job migrates `crowdrelay` once, clones it into `ci_<suite>` per
//! test target, and points every `*_TEST_DATABASE_URL` at that clone. This
//! helper clones that already-migrated database per *test* — `WITH TEMPLATE`
//! is a metadata copy, so a test gets a private database in about a second
//! while migrations run exactly once per job.
//!
//! The previous shape paid full migration cost per test — an empty database
//! plus ~300 migrations each — which is what pushed the job toward an hour.

use sqlx::{Connection, PgConnection, PgPool, Row, postgres::PgPoolOptions};
use tokio::sync::OnceCell;
use uuid::Uuid;

static SWEEPED: OnceCell<()> = OnceCell::const_new();

/// A private clone of the suite database named by `env_var`.
///
/// The migrate call re-checks the applied set on the clone — a no-op when the
/// template is current, and the guard against a stale or unmigrated one.
pub async fn test_pool(env_var: &str) -> Result<PgPool, sqlx::Error> {
    Ok(test_pool_with_url(env_var).await?.0)
}

/// Same clone as [`test_pool`], but also returns the clone's URL for suites
/// whose repository builds a second connection from a `DatabaseConfig`.
pub async fn test_pool_with_url(env_var: &str) -> Result<(PgPool, String), sqlx::Error> {
    let url = std::env::var(env_var).map_err(|e| {
        sqlx::Error::Configuration(
            format!("{env_var} must target the migrated suite database: {e}").into(),
        )
    })?;
    let (prefix, template) = url
        .rsplit_once('/')
        .ok_or_else(|| sqlx::Error::Configuration("database URL has no database name".into()))?;
    let admin_url = format!("{prefix}/postgres");
    let mut admin = PgConnection::connect(&admin_url).await?;

    // Clone names carry a hash of the template (which parent a clone came
    // from) plus a uuid v7 (when it was made), so the sweep can read a
    // clone's age off its name without touching pg_stat_file.
    let sig = {
        let mut h = 0xcbf29ce484222325u64;
        for b in template.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        format!("{:08x}", h & 0xffff_ffff)
    };

    // Once per test binary: drop clone leftovers older than a few hours so a
    // crashed job cannot leak databases on a persistent dev postgres. The
    // sweep covers every clone shape this helper has ever used — `pgt_*` plus
    // the retired `crowdrelay_<tag>_<uuid>` and `ci_<uuid>` schemes — because
    // each name carries a uuid v7 the sweep can age off without touching
    // pg_stat_file. A live clone is minutes old, so nothing past four hours
    // can be in use, regardless of which lane owns it; names without a
    // trailing uuid fail the parse and are never dropped.
    const MAX_CLONE_AGE_SECS: u64 = 4 * 60 * 60;
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    SWEEPED
        .get_or_try_init(|| async {
            let stale: Vec<String> = sqlx::query(
                "SELECT datname FROM pg_database \
                 WHERE datname LIKE 'pgt\\_%' \
                    OR datname LIKE 'crowdrelay\\_%' \
                    OR datname LIKE 'ci\\_%'",
            )
            .fetch_all(&mut admin)
            .await?
            .iter()
            .map(|row| row.get("datname"))
            .collect();
            for datname in stale {
                let old_enough = datname
                    .rsplit('_')
                    .next()
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .and_then(|u| u.get_timestamp().map(|t| t.to_unix().0))
                    .is_some_and(|ts| now_unix.saturating_sub(ts) > MAX_CLONE_AGE_SECS);
                if !old_enough {
                    continue;
                }
                let _ = sqlx::query(&format!(
                    "DROP DATABASE IF EXISTS \"{datname}\" WITH (FORCE)"
                ))
                .execute(&mut admin)
                .await;
            }
            Ok::<(), sqlx::Error>(())
        })
        .await?;

    let name = format!("pgt_{sig}_{}", Uuid::now_v7().simple());
    // A clone refuses while the template has open connections; the suites
    // never hold one, but a straggler pool from a finished test can.
    for attempt in 0..20u8 {
        let result = sqlx::query(&format!(
            "CREATE DATABASE \"{name}\" WITH TEMPLATE \"{template}\""
        ))
        .execute(&mut admin)
        .await;
        match result {
            Ok(_) => break,
            Err(e) => {
                if !e.to_string().contains("being accessed") || attempt == 19 {
                    return Err(e);
                }
                let _ = sqlx::query(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                     WHERE datname = $1 AND pid <> pg_backend_pid()",
                )
                .bind(template)
                .execute(&mut admin)
                .await;
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
    }
    drop(admin);

    let clone_url = format!("{prefix}/{name}");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&clone_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    Ok((pool, clone_url))
}
