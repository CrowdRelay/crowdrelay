//! Fanbase persistence: audience blocks, their ingestion ledger and the
//! membership mapping back to fans.
//!
//! Ingestion reuses the pilot-import consent machinery verbatim: every
//! candidate lands `pending` behind the canonical confirmation email (resends
//! honour the same cooldown the interactive flow uses), active fans are
//! counted but never downgraded, opt-outs are skipped, each member is
//! attributed to its source via the external-id ledger, and the batch closes
//! atomically with its ingestion ledger row.

use sqlx::PgPool;
use uuid::Uuid;

use crowdrelay_domain::fanbase::{AdmissionAction, SourceKind, admission_for};

#[derive(Clone)]
pub struct PostgresFanbaseRepository {
    pool: PgPool,
}

#[derive(Debug, thiserror::Error)]
pub enum FanbaseError {
    #[error("fanbase not found")]
    NotFound,
    /// A fanbase with this name already exists in the workspace.
    #[error("fanbase name already taken")]
    NameTaken,
    /// A connection with this platform + external account already exists.
    #[error("connection already exists")]
    ConnectionExists,
    #[error("fanbase database operation failed")]
    Database(sqlx::Error),
}

#[derive(Debug, sqlx::FromRow)]
pub struct FanbaseRow {
    pub id: Uuid,
    pub name: String,
    pub source_kind: String,
    pub fetch_url: Option<String>,
    pub consent_attested_by: Option<String>,
    pub enabled: bool,
    pub created_at: time::OffsetDateTime,
    // Latest completed ingestion stats (NULL when never ingested).
    pub last_status: Option<String>,
    pub last_finished_at: Option<time::OffsetDateTime>,
    pub last_imported_pending: Option<i32>,
    pub members: Option<i64>,
}

/// One validated candidate from a provider batch.
#[derive(Debug, Clone)]
pub struct FanbaseEntry {
    pub external_id: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub locale: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IngestionCounts {
    pub received: u32,
    pub imported_pending: u32,
    pub confirmation_resent: u32,
    pub already_active: u32,
    pub skipped_suppressed: u32,
    pub cooldown_skipped: u32,
    pub invalid: u32,
}

impl PostgresFanbaseRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    fn unexpected(error: sqlx::Error) -> FanbaseError {
        tracing::error!(error = %error, back = ?std::backtrace::Backtrace::force_capture(), "fanbase persistence failed");
        FanbaseError::Database(error)
    }

    /// Registers an audience block. The name is unique per workspace so the
    /// operator surface can address fanbases by label safely.
    pub async fn create_fanbase(
        &self,
        workspace_id: Uuid,
        name: &str,
        source_kind: SourceKind,
        fetch_url: Option<&str>,
        consent_attested_by: Option<&str>,
    ) -> Result<Uuid, FanbaseError> {
        let id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO fanbases (
                workspace_id, name, source_kind, fetch_url, consent_attested_by
            )
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (workspace_id, name) DO NOTHING
            RETURNING id
            "#,
        )
        .bind(workspace_id)
        .bind(name.trim())
        .bind(source_kind.as_str())
        .bind(fetch_url)
        .bind(consent_attested_by)
        .fetch_optional(&self.pool)
        .await
        .map_err(Self::unexpected)?
        .ok_or(FanbaseError::NameTaken)?;
        Ok(id)
    }

    /// Lists fanbases with their latest completed ingestion stats and member
    /// counts — one purpose-built read model for the operator panel.
    pub async fn list_fanbases(&self, workspace_id: Uuid) -> Result<Vec<FanbaseRow>, FanbaseError> {
        sqlx::query_as::<_, FanbaseRow>(
            r#"
            SELECT fb.id, fb.name, fb.source_kind, fb.fetch_url,
                   fb.consent_attested_by, fb.enabled, fb.created_at,
                   ing.status AS last_status,
                   ing.finished_at AS last_finished_at,
                   ing.imported_pending AS last_imported_pending,
                   (SELECT count(*)::bigint FROM fanbase_members m
                     WHERE m.fanbase_id = fb.id) AS members
            FROM fanbases fb
            LEFT JOIN LATERAL (
                SELECT status, finished_at, imported_pending
                FROM fanbase_ingestions i
                WHERE i.fanbase_id = fb.id AND i.status = 'completed'
                ORDER BY i.started_at DESC LIMIT 1
            ) ing ON true
            WHERE fb.workspace_id = $1
            ORDER BY fb.created_at DESC, fb.id
            "#,
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(Self::unexpected)
    }

    async fn assert_fanbase(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        workspace_id: Uuid,
        fanbase_id: Uuid,
    ) -> Result<(), FanbaseError> {
        let found = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM fanbases WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id)
        .bind(fanbase_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(Self::unexpected)?;
        if found.is_none() {
            return Err(FanbaseError::NotFound);
        }
        Ok(())
    }

    /// Runs one ingestion batch. Per candidate the admission policy decides:
    /// create pending (+ canonical confirmation email), resend within cooldown,
    /// count as already-active, or skip an opt-out. Membership is attributed
    /// per external id either way, and the ledger row closes atomically.
    pub async fn ingest_candidates(
        &self,
        workspace_id: Uuid,
        fanbase_id: Uuid,
        entries: &[FanbaseEntry],
        access_token_ttl_days: i64,
        resend_cooldown_seconds: i64,
    ) -> Result<IngestionCounts, FanbaseError> {
        let mut tx = self.pool.begin().await.map_err(Self::unexpected)?;
        Self::assert_fanbase(&mut tx, workspace_id, fanbase_id).await?;

        let run_id = match sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO fanbase_ingestions (workspace_id, fanbase_id, status)
            VALUES ($1, $2, 'running')
            RETURNING id
            "#,
        )
        .bind(workspace_id)
        .bind(fanbase_id)
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(Some(id)) => id,
            Ok(None) => return Err(FanbaseError::NotFound),
            Err(e) => return Err(FanbaseError::Database(e)),
        };

        let mut counts = IngestionCounts {
            received: entries.len() as u32,
            ..Default::default()
        };
        let batch_request_id = format!(
            "fanbase-ingest-{}-{}",
            fanbase_id.simple(),
            Uuid::now_v7().simple()
        );

        for entry in entries {
            let Some(email) = entry
                .email
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
            else {
                counts.invalid += 1;
                continue;
            };

            let existing = sqlx::query_scalar::<_, String>(
                r#"
                SELECT status FROM fans
                WHERE workspace_id = $1 AND normalized_email = $2
                FOR UPDATE
                "#,
            )
            .bind(workspace_id)
            .bind(email)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Self::unexpected)?;

            let action = admission_for(existing.as_deref());
            match action {
                AdmissionAction::AlreadyActive => counts.already_active += 1,
                AdmissionAction::SkipSuppressed => counts.skipped_suppressed += 1,
                AdmissionAction::CreatePending => {
                    sqlx::query(
                        r#"
                        INSERT INTO fans (workspace_id, normalized_email, display_name, locale, status)
                        VALUES ($1, $2, $3, $4, 'pending')
                        "#,
                    )
                    .bind(workspace_id)
                    .bind(email)
                    .bind(&entry.display_name)
                    .bind(&entry.locale)
                    .execute(&mut *tx)
                    .await
                    .map_err(Self::unexpected)?;
                    counts.imported_pending += 1;
                }
                AdmissionAction::ResendPending => counts.confirmation_resent += 1,
            }

            let fan_id = sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM fans WHERE workspace_id = $1 AND normalized_email = $2",
            )
            .bind(workspace_id)
            .bind(email)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Self::unexpected)?;
            let Some(fan_id) = fan_id else {
                tracing::warn!(email, "fan row vanished after admission");
                counts.invalid += 1;
                continue;
            };

            sqlx::query(
                r#"
                INSERT INTO fanbase_members (
                    workspace_id, fanbase_id, fan_id, external_id
                )
                VALUES ($1, $2, $3, $4)
                ON CONFLICT (fanbase_id, external_id) DO UPDATE SET
                    last_seen_at = now(), fan_id = EXCLUDED.fan_id
                "#,
            )
            .bind(workspace_id)
            .bind(fanbase_id)
            .bind(fan_id)
            .bind(&entry.external_id)
            .execute(&mut *tx)
            .await
            .map_err(Self::unexpected)?;

            // Confirmation email only for fresh pendings and lapsed resends;
            // everything already holding a live window is counted and left.
            if !matches!(
                action,
                AdmissionAction::CreatePending | AdmissionAction::ResendPending
            ) {
                continue;
            }
            let in_cooldown = existing.as_deref() == Some("pending")
                && sqlx::query_scalar::<_, bool>(
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
                .map_err(Self::unexpected)?;
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
            .map_err(Self::unexpected)?;

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
            .map_err(Self::unexpected)?;

            let event_payload = serde_json::json!({
                "workspace_id": workspace_id,
                "fan_id": fan_id,
                "email": email,
                "display_name": entry.display_name,
                "locale": entry.locale,
                "confirmation_token": raw_token,
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
            .bind(format!("{batch_request_id}:{}", counts.received))
            .execute(&mut *tx)
            .await
            .map_err(Self::unexpected)?;
        }

        sqlx::query(
            r#"
            UPDATE fanbase_ingestions
            SET status = 'completed',
                received = $2, imported_pending = $3,
                already_active = $4, skipped_suppressed = $5,
                cooldown_skipped = $6, invalid = $7,
                confirmation_resent = $8,
                finished_at = now()
            WHERE id = $1
            "#,
        )
        .bind(run_id)
        .bind(i32::try_from(counts.received).unwrap_or(i32::MAX))
        .bind(i32::try_from(counts.imported_pending).unwrap_or(i32::MAX))
        .bind(i32::try_from(counts.already_active).unwrap_or(i32::MAX))
        .bind(i32::try_from(counts.skipped_suppressed).unwrap_or(i32::MAX))
        .bind(i32::try_from(counts.cooldown_skipped).unwrap_or(i32::MAX))
        .bind(i32::try_from(counts.invalid).unwrap_or(i32::MAX))
        .bind(i32::try_from(counts.confirmation_resent).unwrap_or(i32::MAX))
        .execute(&mut *tx)
        .await
        .map_err(Self::unexpected)?;

        tx.commit().await.map_err(Self::unexpected)?;
        Ok(counts)
    }
}

// ---------------------------------------------------------------------------
// Fanbase connections — OAuth-linked platform accounts.
//
// A connection records that a workspace has authorized access to an external
// platform account. The credential itself lives outside this database (in
// n8n's encrypted credential store for Path B); `credential_ref` is the
// opaque handle the sync layer uses to resolve it at runtime.
// ---------------------------------------------------------------------------

#[derive(Debug, sqlx::FromRow)]
pub struct ConnectionRow {
    pub id: Uuid,
    pub platform: String,
    pub external_account_ref: String,
    pub credential_ref: String,
    pub status: String,
    pub label: String,
    pub last_sync_at: Option<time::OffsetDateTime>,
    pub created_at: time::OffsetDateTime,
}

impl PostgresFanbaseRepository {
    pub async fn list_connections(
        &self,
        workspace_id: Uuid,
    ) -> Result<Vec<ConnectionRow>, FanbaseError> {
        sqlx::query_as::<_, ConnectionRow>(
            r#"
            SELECT id, platform, external_account_ref, credential_ref,
                   status, label, last_sync_at, created_at
            FROM fanbase_connections
            WHERE workspace_id = $1
            ORDER BY created_at, label
            "#,
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(Self::unexpected)
    }

    pub async fn create_connection(
        &self,
        workspace_id: Uuid,
        platform: &str,
        external_account_ref: &str,
        credential_ref: &str,
        label: &str,
    ) -> Result<Uuid, FanbaseError> {
        let id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO fanbase_connections (
                workspace_id, platform, external_account_ref,
                credential_ref, label, status
            )
            VALUES ($1, $2, $3, $4, $5, 'connected')
            ON CONFLICT (workspace_id, platform, external_account_ref) DO NOTHING
            RETURNING id
            "#,
        )
        .bind(workspace_id)
        .bind(platform)
        .bind(external_account_ref.trim())
        .bind(credential_ref.trim())
        .bind(label.trim())
        .fetch_optional(&self.pool)
        .await
        .map_err(Self::unexpected)?
        .ok_or(FanbaseError::ConnectionExists)?;
        Ok(id)
    }

    pub async fn update_connection_status(
        &self,
        workspace_id: Uuid,
        connection_id: Uuid,
        status: &str,
    ) -> Result<(), FanbaseError> {
        let affected = sqlx::query(
            r#"
            UPDATE fanbase_connections
            SET status = $3, updated_at = now()
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(workspace_id)
        .bind(connection_id)
        .bind(status)
        .execute(&self.pool)
        .await
        .map_err(Self::unexpected)?;
        if affected.rows_affected() == 0 {
            return Err(FanbaseError::NotFound);
        }
        Ok(())
    }

    pub async fn delete_connection(
        &self,
        workspace_id: Uuid,
        connection_id: Uuid,
    ) -> Result<(), FanbaseError> {
        let affected =
            sqlx::query("DELETE FROM fanbase_connections WHERE workspace_id = $1 AND id = $2")
                .bind(workspace_id)
                .bind(connection_id)
                .execute(&self.pool)
                .await
                .map_err(Self::unexpected)?;
        if affected.rows_affected() == 0 {
            return Err(FanbaseError::NotFound);
        }
        Ok(())
    }

    pub async fn touch_connection_sync(
        &self,
        workspace_id: Uuid,
        connection_id: Uuid,
    ) -> Result<(), FanbaseError> {
        sqlx::query(
            r#"
            UPDATE fanbase_connections
            SET last_sync_at = now(), updated_at = now()
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(workspace_id)
        .bind(connection_id)
        .execute(&self.pool)
        .await
        .map_err(Self::unexpected)?;
        Ok(())
    }
}
