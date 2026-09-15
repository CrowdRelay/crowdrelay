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

/// Records that an installation now belongs to an identified fan.
///
/// This is the second half of the funnel migration 0257 was written to measure:
/// "the gap between the count of rows and the count of non-null values here IS
/// the activation funnel." The first half — recording the install before anyone
/// is identified — shipped. Nothing ever wrote `fan_id`, so the numerator was
/// zero by construction and the funnel could only ever read 0%.
///
/// Called from the push-endpoint registration, which is the moment an install
/// stops being anonymous: `/v1/me/push/endpoints` requires a fan session and its
/// request already carries the app's `installation_id`, so both halves are in
/// hand there and nowhere earlier.
///
/// Set only while `fan_id` is null. The column answers "did this install ever
/// convert", and the first identification is what converted it — a device later
/// handed to somebody else must not rewrite that history. Live push targeting
/// reads `fan_push_endpoints`, which the same request updates unconditionally,
/// so nothing about delivery depends on this staying fixed.
///
/// # Errors
///
/// Returns the `sqlx` error. The caller must not fail the registration over it:
/// the endpoint's job is to accept a push registration, and losing a funnel
/// datapoint is not a reason to refuse one.
pub async fn link_installation_to_fan(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    installation_id: &str,
    fan_id: uuid::Uuid,
) -> Result<bool, sqlx::Error> {
    let linked = sqlx::query(
        r#"
        UPDATE signal_installations
        SET fan_id = $3,
            last_seen_at = now()
        WHERE workspace_id = $1
          AND installation_id = $2
          AND fan_id IS NULL
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(installation_id)
    .bind(fan_id)
    .execute(pool)
    .await?
    .rows_affected();

    // The install's owner can differ from the fan registering right now —
    // a device handed to somebody else keeps its first identification. That
    // second presentation is itself a merge candidate for a human to weigh.
    if linked == 0 {
        match sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT fan_id FROM signal_installations \
             WHERE workspace_id = $1 AND installation_id = $2 AND fan_id <> $3",
        )
        .bind(workspace_id.into_uuid())
        .bind(installation_id)
        .bind(fan_id)
        .fetch_optional(pool)
        .await
        {
            Ok(Some(other)) => {
                if let Err(error) = crate::fan_identity::record_merge_candidate(
                    pool,
                    workspace_id.into_uuid(),
                    other,
                    fan_id,
                    serde_json::json!({
                        "kind": "shared_signal_install",
                        "installation_id": installation_id,
                        "presentation": "second_fan",
                    }),
                )
                .await
                {
                    tracing::warn!(%error, "could not record shared-install merge candidate");
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, "could not read install owner for merge candidate");
            }
        }
    }

    // Identity spine, §4e-5: an identified install is a verified device
    // identifier. It attaches to the install's owner — the first fan it
    // identified as, matching the `fan_id IS NULL` rule above — and when the
    // identifier is already owned by a different fan the same device just
    // presented as two people: a merge candidate, never an automatic merge.
    let owner = sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT fan_id FROM signal_installations WHERE workspace_id = $1 AND installation_id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(installation_id)
    .fetch_optional(pool)
    .await?;
    if let Some(owner) = owner {
        let identifier_owner = sqlx::query_scalar::<_, uuid::Uuid>(
            "WITH ins AS ( \
                 INSERT INTO fan_identifiers (workspace_id, fan_id, kind, value, source, verified_at) \
                 VALUES ($1, $2, 'signal_install', $3, 'push_endpoint_registration', now()) \
                 ON CONFLICT (workspace_id, kind, value) DO NOTHING RETURNING fan_id) \
             SELECT fan_id FROM ins UNION ALL \
             SELECT fan_id FROM fan_identifiers \
             WHERE workspace_id = $1 AND kind = 'signal_install' AND value = $3 \
             LIMIT 1",
        )
        .bind(workspace_id.into_uuid())
        .bind(owner)
        .bind(installation_id)
        .fetch_one(pool)
        .await?;
        if identifier_owner != owner
            && let Err(error) = crate::fan_identity::record_merge_candidate(
                pool,
                workspace_id.into_uuid(),
                owner,
                identifier_owner,
                serde_json::json!({
                    "kind": "shared_signal_install",
                    "installation_id": installation_id,
                }),
            )
            .await
        {
            tracing::warn!(%error, "could not record shared-install merge candidate");
        }
    }
    Ok(linked > 0)
}
