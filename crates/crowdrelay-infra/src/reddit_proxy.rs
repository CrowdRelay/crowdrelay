//! Reddit proxy state — reads the proxy URL discovered by the
//! `reddit-proxy-finder` sidecar (Python `free-proxy` library).
//!
//! The sidecar writes a singleton row to `reddit_proxy_state` every 15
//! minutes. The worker reads it every 5 minutes and dynamically rebuilds
//! its HTTP client when the proxy changes. This bypasses Reddit's
//! IP-level 403 blocks without a restart.

use sqlx::PgPool;

/// Reads the current Reddit proxy from the database. Returns `None` if no
/// proxy has been written yet, or if the last verification is older than 1
/// hour (stale proxy is likely dead).
///
/// # Errors
/// Returns `None` on any database error — the caller falls back to the
/// env var proxy or direct connection.
pub async fn read_reddit_proxy_from_db(pool: &PgPool) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT proxy_url FROM reddit_proxy_state \
         WHERE id = 1 \
           AND last_verified_at > now() - interval '1 hour' \
           AND last_verified_ok = true",
    )
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        tracing::warn!(error = %error, "failed to read reddit_proxy_state — falling back to env or direct");
        error
    })
    .ok()
    .flatten()
    .filter(|url| !url.is_empty())
}
