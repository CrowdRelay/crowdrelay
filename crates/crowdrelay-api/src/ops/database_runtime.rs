use serde::Serialize;
use sqlx::{FromRow, PgPool};

#[derive(Debug, Serialize)]
pub(super) struct DatabaseRuntimeSummary {
    pool_size: u32,
    pool_idle: u32,
    pool_max: u32,
    server_version_num: i32,
    io_method: Option<String>,
    io_workers: Option<i32>,
    io_max_concurrency: Option<i32>,
    effective_io_concurrency: Option<i32>,
    maintenance_io_concurrency: Option<i32>,
    io_combine_limit_bytes: Option<i64>,
    io_max_combine_limit_bytes: Option<i64>,
    async_io_active: bool,
}

#[derive(Debug, FromRow)]
pub(super) struct DatabaseRuntimeRow {
    server_version_num: i32,
    io_method: Option<String>,
    io_workers: Option<i32>,
    io_max_concurrency: Option<i32>,
    effective_io_concurrency: Option<i32>,
    maintenance_io_concurrency: Option<i32>,
    io_combine_limit_bytes: Option<i64>,
    io_max_combine_limit_bytes: Option<i64>,
}

impl DatabaseRuntimeSummary {
    pub(super) fn from_row(pool: &PgPool, row: DatabaseRuntimeRow) -> Self {
        Self {
            pool_size: pool.size(),
            pool_idle: u32::try_from(pool.num_idle()).unwrap_or(u32::MAX),
            pool_max: pool.options().get_max_connections(),
            server_version_num: row.server_version_num,
            async_io_active: row.server_version_num >= 180_000
                && row.effective_io_concurrency.unwrap_or_default() > 0
                && row.io_method.as_deref() != Some("sync"),
            io_method: row.io_method,
            io_workers: row.io_workers,
            io_max_concurrency: row.io_max_concurrency,
            effective_io_concurrency: row.effective_io_concurrency,
            maintenance_io_concurrency: row.maintenance_io_concurrency,
            io_combine_limit_bytes: row.io_combine_limit_bytes,
            io_max_combine_limit_bytes: row.io_max_combine_limit_bytes,
        }
    }
}
