//! Persistence for Signal app installations.
//!
//! The write lives here rather than beside its HTTP handler because
//! `api-sql-ratchet` forbids new mutations in the transport layer, and it is
//! right to: a handler that owns its own SQL is a handler nobody can call from
//! anywhere else, and the layering test exists to keep that from spreading.
//!
//! What is recorded, and why it is recorded before anyone is identified, is
//! documented on the endpoint in `crowdrelay-api`.

use sqlx::PgPool;

use crowdrelay_domain::WorkspaceId;

/// Records that an installation exists, or that an existing one is still
/// alive.
///
/// Idempotent on `(workspace_id, installation_id)` so the app may call it on
/// every launch. That repetition is the point: it keeps `last_seen_at`
/// meaningful, and an app still opening today is a different fact from one
/// installed once and abandoned.
///
/// # Errors
///
/// Returns the `sqlx` error when the row cannot be written. The caller answers
/// 503: failing to count an install must never look like a rejected install.
pub async fn record_installation(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    installation_id: &str,
    platform: &str,
    app_version: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO signal_installations
            (workspace_id, installation_id, platform, app_version)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (workspace_id, installation_id) DO UPDATE
            SET last_seen_at = now(),
                platform     = EXCLUDED.platform,
                -- A launch that does not report its version must not erase the
                -- version the last one did.
                app_version  = COALESCE(EXCLUDED.app_version, signal_installations.app_version)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(installation_id)
    .bind(platform)
    .bind(app_version)
    .execute(pool)
    .await?;
    Ok(())
}
