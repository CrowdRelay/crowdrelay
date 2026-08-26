//! Pilot mailing-list import persistence.
//!
//! Consent comes first: every imported address lands as `pending` and receives
//! the canonical double-opt-in confirmation through the workspace's own outbox.
//! Active fans are never touched, opt-outs are never resurrected, and the
//! whole batch commits atomically with one source-labelled audit row.

use sqlx::PgPool;
use uuid::Uuid;

#[derive(Clone)]
pub struct PostgresFanImportRepository {
    pool: PgPool,
}

#[derive(Debug, Clone)]
pub struct ImportEntry {
    pub email: String,
    pub display_name: Option<String>,
    pub locale: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ImportCounts {
    pub imported_pending: u32,
    pub confirmation_resent: u32,
    pub already_active: u32,
    pub skipped_suppressed: u32,
    pub cooldown_skipped: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum FanImportError {
    #[error("fan import database operation failed")]
    Database(#[from] sqlx::Error),
}

impl PostgresFanImportRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Imports one validated batch. Returns per-outcome counters for the API
    /// response; addresses never appear in any return value.
    pub async fn import_batch(
        &self,
        workspace_id: Uuid,
        source: &str,
        entries: &[ImportEntry],
        access_token_ttl_days: i64,
        resend_cooldown_seconds: i64,
    ) -> Result<ImportCounts, FanImportError> {
        let mut tx = self.pool.begin().await.map_err(FanImportError::Database)?;

        let mut counts = ImportCounts::default();
        let batch_request_id = format!("fan-import-{}", Uuid::now_v7().simple());

        for entry in entries {
            let existing = sqlx::query_scalar::<_, String>(
                r#"
                SELECT status FROM fans
                WHERE workspace_id = $1 AND normalized_email = $2
                FOR UPDATE
                "#,
            )
            .bind(workspace_id)
            .bind(&entry.email)
            .fetch_optional(&mut *tx)
            .await
            .map_err(FanImportError::Database)?;

            let fan_status = match existing {
                Some(status) => status,
                None => {
                    sqlx::query(
                        r#"
                        INSERT INTO fans (workspace_id, normalized_email, display_name, locale, status)
                        VALUES ($1, $2, $3, $4, 'pending')
                        "#,
                    )
                    .bind(workspace_id)
                    .bind(&entry.email)
                    .bind(&entry.display_name)
                    .bind(&entry.locale)
                    .execute(&mut *tx)
                    .await
                    .map_err(FanImportError::Database)?;
                    counts.imported_pending += 1;
                    "pending".to_owned()
                }
            };

            match fan_status.as_str() {
                "active" => {
                    counts.already_active += 1;
                    continue;
                }
                "unsubscribed" | "suppressed" => {
                    counts.skipped_suppressed += 1;
                    continue;
                }
                "pending" => {}
                unexpected => {
                    tracing::error!(status = %unexpected, "unexpected fan status during import");
                    return Err(FanImportError::Database(sqlx::Error::ColumnDecode {
                        index: "status".to_owned(),
                        source: "unexpected fan status during import".into(),
                    }));
                }
            }

            let fan_id = sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM fans WHERE workspace_id = $1 AND normalized_email = $2",
            )
            .bind(workspace_id)
            .bind(&entry.email)
            .fetch_one(&mut *tx)
            .await
            .map_err(FanImportError::Database)?;

            // Same resend cooldown the interactive flow uses so a fresh import
            // cannot machine-gun confirmation emails at an awaiting address.
            let in_cooldown = sqlx::query_scalar::<_, bool>(
                r#"
                SELECT EXISTS (
                    SELECT 1 FROM fan_action_tokens
                    WHERE workspace_id = $1 AND fan_id = $2
                      AND purpose = 'confirm'
                      AND consumed_at IS NULL AND expires_at > now()
                      AND created_at > now() - ($3::bigint * interval '1 second')
                )
                "#,
            )
            .bind(workspace_id)
            .bind(fan_id)
            .bind(resend_cooldown_seconds)
            .fetch_one(&mut *tx)
            .await
            .map_err(FanImportError::Database)?;
            if in_cooldown {
                counts.cooldown_skipped += 1;
                continue;
            }

            sqlx::query(
                r#"
                UPDATE fan_action_tokens
                SET consumed_at = COALESCE(consumed_at, now())
                WHERE workspace_id = $1 AND fan_id = $2
                  AND purpose = 'confirm' AND consumed_at IS NULL
                "#,
            )
            .bind(workspace_id)
            .bind(fan_id)
            .execute(&mut *tx)
            .await
            .map_err(FanImportError::Database)?;

            let raw_token = sqlx::query_scalar::<_, String>(
                r#"
                WITH material AS (
                    SELECT encode(gen_random_bytes(32), 'hex') AS token
                ), inserted AS (
                    INSERT INTO fan_action_tokens (
                        workspace_id, fan_id, purpose, token_hash, expires_at
                    )
                    SELECT $1, $2, 'confirm', digest(material.token, 'sha256'),
                        now() + ($3::bigint * interval '1 day')
                    FROM material
                    RETURNING id
                )
                SELECT material.token FROM material, inserted
                "#,
            )
            .bind(workspace_id)
            .bind(fan_id)
            .bind(access_token_ttl_days)
            .fetch_one(&mut *tx)
            .await
            .map_err(FanImportError::Database)?;

            let event_payload = serde_json::json!({
                "workspace_id": workspace_id,
                "fan_id": fan_id,
                "email": entry.email,
                "display_name": entry.display_name,
                "locale": entry.locale,
                "confirmation_token": raw_token,
                "import_source": source.trim(),
            });
            sqlx::query(
                r#"
                INSERT INTO outbox_events (
                    workspace_id, event_type, event_version, payload, request_id
                ) VALUES ($1, 'fan.confirmation_requested', 1, $2, $3)
                "#,
            )
            .bind(workspace_id)
            .bind(event_payload)
            .bind(format!("{batch_request_id}:{}", counts.confirmation_resent))
            .execute(&mut *tx)
            .await
            .map_err(FanImportError::Database)?;
            counts.confirmation_resent += 1;
        }

        sqlx::query(
            r#"
            INSERT INTO audit_events (
                workspace_id, actor_kind, action, target_type, target_id, metadata
            ) VALUES ($1, 'service', 'fans.imported', 'workspace', $2, $3)
            "#,
        )
        .bind(workspace_id)
        .bind(workspace_id.to_string())
        .bind(serde_json::json!({
            "source": source.trim(),
            "imported_pending": counts.imported_pending,
            "confirmation_resent": counts.confirmation_resent,
            "already_active": counts.already_active,
            "skipped_suppressed": counts.skipped_suppressed,
            "cooldown_skipped": counts.cooldown_skipped,
        }))
        .execute(&mut *tx)
        .await
        .map_err(FanImportError::Database)?;

        tx.commit().await.map_err(FanImportError::Database)?;
        Ok(counts)
    }
}
