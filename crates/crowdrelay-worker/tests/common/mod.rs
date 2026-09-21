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
pub async fn test_pool(env_var: &str) -> Result<PgPool, Box<dyn std::error::Error + Send + Sync>> {
    let url = std::env::var(env_var)
        .map_err(|e| format!("{env_var} must target the migrated suite database: {e}"))?;
    let (prefix, template) = url
        .rsplit_once('/')
        .ok_or("database URL has no database name")?;
    let admin_url = format!("{prefix}/postgres");
    let mut admin = PgConnection::connect(&admin_url).await?;

    // Clone names carry a hash of the template so parallel sessions sharing
    // this postgres only ever sweep their own leftovers — a generic `pgt_%`
    // sweep could FORCE-drop another lane's live clone.
    let sig = {
        let mut h = 0xcbf29ce484222325u64;
        for b in template.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        format!("{:08x}", h & 0xffff_ffff)
    };

    // Once per test binary: drop clone leftovers from earlier runs so a
    // crashed job cannot leak `pgt_*` databases on a persistent dev postgres.
    // A leftover still open belongs to a test running in another lane — the
    // DROP then fails harmlessly and stays for the next sweep.
    SWEEPED
        .get_or_try_init(|| async {
            let stale: Vec<String> = sqlx::query(&format!(
                "SELECT datname FROM pg_database WHERE datname LIKE 'pgt\\_{sig}\\_%'"
            ))
            .fetch_all(&mut admin)
            .await?
            .iter()
            .map(|row| row.get("datname"))
            .collect();
            for datname in stale {
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
                    return Err(e.into());
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

    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&format!("{prefix}/{name}"))
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    Ok(pool)
}
