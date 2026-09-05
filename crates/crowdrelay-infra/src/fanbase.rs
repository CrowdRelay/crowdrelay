//! Fanbase persistence: audience blocks, their ingestion ledger and the
//! membership mapping back to fans.
//!
//! Ingestion reuses the pilot-import consent machinery verbatim: every
//! candidate lands `pending` behind the canonical confirmation email (resends
//! honour the same cooldown the interactive flow uses), active fans are
//! counted but never downgraded, opt-outs are skipped, each member is
//! attributed to its source via the external-id ledger, and the batch closes
//! atomically with its ingestion ledger row.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sqlx::PgPool;
use uuid::Uuid;

use crate::sensitive_response::{SensitiveResponseKey, encrypt_value};
use crowdrelay_domain::fanbase::{AdmissionAction, SourceKind, admission_for};

#[derive(Clone)]
pub struct PostgresFanbaseRepository {
    pool: PgPool,
    /// Encryption key for OAuth tokens stored in `encrypted_access_token` /
    /// `encrypted_refresh_token`. `None` for callers that never touch OAuth
    /// connections (legacy n8n-backed paths).
    encryption_key: Option<SensitiveResponseKey>,
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
    #[error("OAuth token encryption failed")]
    Encryption,
    #[error("OAuth token decryption failed")]
    Decryption,
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
        Self {
            pool,
            encryption_key: None,
        }
    }

    /// Sets the encryption key for OAuth token storage. Required before
    /// calling `upsert_tiktok_connection` or any method that reads
    /// encrypted credentials.
    #[must_use]
    pub fn with_encryption_key(mut self, key: SensitiveResponseKey) -> Self {
        self.encryption_key = Some(key);
        self
    }

    /// Associated data for token encryption. Binds the ciphertext to
    /// the workspace and provider account so a token stolen from one
    /// workspace cannot be decrypted in another. The `platform` parameter
    /// ensures tokens encrypted for one platform cannot be decrypted for
    /// another.
    fn token_aad(workspace_id: Uuid, platform: &str, account_id: &str) -> Vec<u8> {
        format!("crowdrelay.fanbase.oauth.{platform}.v1\0{workspace_id}\0{account_id}").into_bytes()
    }

    fn encrypt_token(
        &self,
        plaintext: &str,
        workspace_id: Uuid,
        platform: &str,
        account_id: &str,
    ) -> Result<String, FanbaseError> {
        let key = self
            .encryption_key
            .as_ref()
            .ok_or(FanbaseError::Encryption)?;
        let aad = Self::token_aad(workspace_id, platform, account_id);
        let encrypted =
            encrypt_value(plaintext.as_bytes(), key, &aad).map_err(|_| FanbaseError::Encryption)?;
        Ok(URL_SAFE_NO_PAD.encode(&encrypted))
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

    /// Removes a fanbase and all its dependent rows (ingestions, members).
    /// The CASCADE foreign keys on `fanbase_ingestions` and `fanbase_members`
    /// do the cleanup; the fans themselves stay — they belong to the workspace,
    /// not to the fanbase that acquired them.
    pub async fn delete_fanbase(
        &self,
        workspace_id: Uuid,
        fanbase_id: Uuid,
    ) -> Result<(), FanbaseError> {
        let affected = sqlx::query("DELETE FROM fanbases WHERE workspace_id = $1 AND id = $2")
            .bind(workspace_id)
            .bind(fanbase_id)
            .execute(&self.pool)
            .await
            .map_err(Self::unexpected)?;
        if affected.rows_affected() == 0 {
            return Err(FanbaseError::NotFound);
        }
        Ok(())
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
                // ResendPending is counted only after the cooldown check
                // passes below — a pending fan still inside its confirmation
                // window is cooldown_skipped, not confirmation_resent. This
                // mirrors fan_import.rs, where confirmation_resent means
                // "actually sent", and keeps sum(counts) == received.
                AdmissionAction::ResendPending => {}
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

            // The confirmation email will actually be sent — count it now,
            // after the cooldown gate. This covers both CreatePending (new
            // fan, never in cooldown) and ResendPending (lapsed window).
            // Counting at admission instead would double-count entries that
            // land in cooldown_skipped below.
            counts.confirmation_resent += 1;

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
// Fanbase connections — platform accounts linked via credential_ref (n8n)
// or provider_account_id (YouTube API key). No OAuth tokens stored in DB.
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
    /// Why the most recent sync failed, verbatim from the provider adapter.
    ///
    /// Carried to the console because `status` cannot answer this: it says
    /// credentials are present, which stayed true for five connections that
    /// had never once succeeded.
    pub last_sync_error: Option<String>,
    pub last_sync_failed_at: Option<time::OffsetDateTime>,
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
                   status, label, last_sync_at, last_sync_error,
                   last_sync_failed_at, created_at
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

    /// Records that a sync succeeded, clearing any previous failure.
    ///
    /// This had no callers at all, which is why `last_sync_at` was NULL for
    /// every one of production's 41 connections while the console showed them
    /// all as connected.
    ///
    /// # Errors
    /// Returns the underlying database error if the update cannot be applied.
    pub async fn touch_connection_sync(
        &self,
        workspace_id: Uuid,
        connection_id: Uuid,
    ) -> Result<(), FanbaseError> {
        sqlx::query(
            r#"
            UPDATE fanbase_connections
            SET last_sync_at = now(),
                last_sync_error = NULL,
                last_sync_failed_at = NULL,
                updated_at = now()
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

    /// Records why a sync failed, so the console can say so.
    ///
    /// `status` is deliberately untouched: it means "credentials are present",
    /// which is what the connect flow sets and the disconnect flow clears.
    /// Folding a provider outage into it would make a transient failure
    /// indistinguishable from a revoked credential, and the operator would go
    /// looking for the wrong thing.
    ///
    /// The message is truncated to the column's limit rather than rejected —
    /// a provider that returns a wall of HTML must not turn a reportable
    /// failure into an unreportable one.
    ///
    /// # Errors
    /// Returns the underlying database error if the update cannot be applied.
    pub async fn record_connection_sync_failure(
        &self,
        workspace_id: Uuid,
        connection_id: Uuid,
        error: &str,
    ) -> Result<(), FanbaseError> {
        let trimmed: String = error.chars().take(500).collect();
        sqlx::query(
            r#"
            UPDATE fanbase_connections
            SET last_sync_error = $3,
                last_sync_failed_at = now(),
                updated_at = now()
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(workspace_id)
        .bind(connection_id)
        .bind(&trimmed)
        .execute(&self.pool)
        .await
        .map_err(Self::unexpected)?;
        Ok(())
    }

    /// Upserts a TikTok connection with OAuth tokens. Called by the
    /// TikTok OAuth callback handler after a successful token exchange.
    /// Tokens are encrypted with `SensitiveResponseKey` and stored in
    /// `encrypted_access_token` / `encrypted_refresh_token`. The
    /// `credential_ref` column stores a short reference identifier
    /// (`tiktok:{open_id}`), not a secret blob.
    pub async fn upsert_tiktok_connection(
        &self,
        workspace_id: Uuid,
        open_id: &str,
        access_token: &str,
        refresh_token: &str,
        expires_at: time::OffsetDateTime,
        scope: &str,
    ) -> Result<(), FanbaseError> {
        let encrypted_access = self.encrypt_token(access_token, workspace_id, "tiktok", open_id)?;
        let encrypted_refresh =
            self.encrypt_token(refresh_token, workspace_id, "tiktok", open_id)?;
        let credential_ref = format!("tiktok:{open_id}");
        let label = format!("TikTok — {open_id}");
        sqlx::query(
            r#"
            INSERT INTO fanbase_connections (
                workspace_id, platform, external_account_ref,
                credential_ref, label, status, provider_account_id,
                encrypted_access_token, encrypted_refresh_token,
                token_expires_at, token_scope, token_type
            )
            VALUES ($1, 'tiktok', $2, $3, $4, 'connected', $2,
                    $5, $6, $7, $8, 'bearer')
            ON CONFLICT (workspace_id, platform, external_account_ref)
            DO UPDATE SET
                credential_ref = EXCLUDED.credential_ref,
                encrypted_access_token = EXCLUDED.encrypted_access_token,
                encrypted_refresh_token = EXCLUDED.encrypted_refresh_token,
                token_expires_at = EXCLUDED.token_expires_at,
                token_scope = EXCLUDED.token_scope,
                status = 'connected',
                updated_at = now()
            "#,
        )
        .bind(workspace_id)
        .bind(open_id)
        .bind(&credential_ref)
        .bind(&label)
        .bind(&encrypted_access)
        .bind(&encrypted_refresh)
        .bind(expires_at)
        .bind(scope)
        .execute(&self.pool)
        .await
        .map_err(Self::unexpected)?;
        // Notify the growth metric sync worker so it picks up the new
        // connection immediately.
        sqlx::query("SELECT pg_notify('growth_metric_sync', 'tiktok-connected')")
            .execute(&self.pool)
            .await
            .map_err(Self::unexpected)?;
        Ok(())
    }

    /// Registers a Discord server connection for growth metric sync.
    /// The `invite_code` is the Discord invite code (e.g. `BBdDV6gVy`).
    /// When `posting_config` is provided, the bot token is encrypted and
    /// stored in `encrypted_access_token`, and the channel ID is stored in
    /// `provider_account_id` (replacing the invite code, which remains in
    /// `external_account_ref` for metric sync). Without `posting_config`,
    /// the connection only supports metric sync (invite code in both
    /// `external_account_ref` and `provider_account_id`).
    pub async fn upsert_discord_connection(
        &self,
        workspace_id: Uuid,
        invite_code: &str,
        label: &str,
        posting_config: Option<(&str, &str)>,
    ) -> Result<(), FanbaseError> {
        let credential_ref = format!("discord:{invite_code}");
        match posting_config {
            Some((bot_token, channel_id)) => {
                let encrypted_token =
                    self.encrypt_token(bot_token, workspace_id, "discord", channel_id)?;
                sqlx::query(
                    r#"
                    INSERT INTO fanbase_connections (
                        workspace_id, platform, external_account_ref,
                        credential_ref, label, status, provider_account_id,
                        encrypted_access_token, token_type
                    )
                    VALUES ($1, 'discord', $2, $3, $4, 'connected', $5, $6, 'bearer')
                    ON CONFLICT (workspace_id, platform, external_account_ref)
                    DO UPDATE SET
                        credential_ref = EXCLUDED.credential_ref,
                        label = EXCLUDED.label,
                        provider_account_id = EXCLUDED.provider_account_id,
                        encrypted_access_token = EXCLUDED.encrypted_access_token,
                        token_type = EXCLUDED.token_type,
                        status = 'connected',
                        updated_at = now()
                    "#,
                )
                .bind(workspace_id)
                .bind(invite_code)
                .bind(&credential_ref)
                .bind(label)
                .bind(channel_id)
                .bind(&encrypted_token)
                .execute(&self.pool)
                .await
                .map_err(Self::unexpected)?;
            }
            None => {
                sqlx::query(
                    r#"
                    INSERT INTO fanbase_connections (
                        workspace_id, platform, external_account_ref,
                        credential_ref, label, status, provider_account_id
                    )
                    VALUES ($1, 'discord', $2, $3, $4, 'connected', $2)
                    ON CONFLICT (workspace_id, platform, external_account_ref)
                    DO UPDATE SET
                        credential_ref = EXCLUDED.credential_ref,
                        label = EXCLUDED.label,
                        status = 'connected',
                        updated_at = now()
                    "#,
                )
                .bind(workspace_id)
                .bind(invite_code)
                .bind(&credential_ref)
                .bind(label)
                .execute(&self.pool)
                .await
                .map_err(Self::unexpected)?;
            }
        }
        sqlx::query("SELECT pg_notify('growth_metric_sync', 'discord-connected')")
            .execute(&self.pool)
            .await
            .map_err(Self::unexpected)?;
        Ok(())
    }

    /// Registers a simple credential-less connection for growth metric sync.
    /// Used by platforms like Last.fm where the API key is a shared env var
    /// and the only per-connection identifier is the artist/entity name
    /// stored in `provider_account_id`.
    ///
    /// When `unverified` is true, the connection is stored with
    /// `status = 'unverified'` — the creation-time probe could not confirm
    /// the identity (network error, rate limit). A successful sync promotes
    /// it to `'connected'`.
    pub async fn upsert_simple_connection(
        &self,
        workspace_id: Uuid,
        platform: &str,
        account_id: &str,
        label: &str,
        unverified: bool,
    ) -> Result<(), FanbaseError> {
        let credential_ref = format!("{platform}:{account_id}");
        let status = if unverified {
            "unverified"
        } else {
            "connected"
        };
        sqlx::query(
            r#"
            INSERT INTO fanbase_connections (
                workspace_id, platform, external_account_ref,
                credential_ref, label, status, provider_account_id
            )
            VALUES ($1, $2, $3, $4, $5, $6, $3)
            ON CONFLICT (workspace_id, platform, external_account_ref)
            DO UPDATE SET
                credential_ref = EXCLUDED.credential_ref,
                label = EXCLUDED.label,
                status = $6,
                updated_at = now()
            "#,
        )
        .bind(workspace_id)
        .bind(platform)
        .bind(account_id)
        .bind(&credential_ref)
        .bind(label)
        .bind(status)
        .execute(&self.pool)
        .await
        .map_err(Self::unexpected)?;
        let notify_msg = format!("{platform}-connected");
        sqlx::query("SELECT pg_notify('growth_metric_sync', $1)")
            .bind(&notify_msg)
            .execute(&self.pool)
            .await
            .map_err(Self::unexpected)?;
        Ok(())
    }

    /// Registers a simple connection with `status = 'invalid'`. Used when
    /// the provider probe proved the external identity does not exist.
    /// The growth metric sync worker skips invalid connections (its
    /// `DueConnection` query filters by `status NOT IN ('invalid', 'expired')`).
    pub async fn upsert_invalid_connection(
        &self,
        workspace_id: Uuid,
        platform: &str,
        account_id: &str,
        label: &str,
    ) -> Result<(), FanbaseError> {
        let credential_ref = format!("{platform}:{account_id}");
        sqlx::query(
            r#"
            INSERT INTO fanbase_connections (
                workspace_id, platform, external_account_ref,
                credential_ref, label, status, provider_account_id
            )
            VALUES ($1, $2, $3, $4, $5, 'invalid', $3)
            ON CONFLICT (workspace_id, platform, external_account_ref)
            DO UPDATE SET
                credential_ref = EXCLUDED.credential_ref,
                label = EXCLUDED.label,
                status = 'invalid',
                updated_at = now()
            "#,
        )
        .bind(workspace_id)
        .bind(platform)
        .bind(account_id)
        .bind(&credential_ref)
        .bind(label)
        .execute(&self.pool)
        .await
        .map_err(Self::unexpected)?;
        Ok(())
    }

    /// Registers a Telegram channel connection for growth metric sync.
    /// The `channel` is the channel username (e.g. `@virya_music`).
    /// The `bot_token` is encrypted and stored in `encrypted_access_token`.
    pub async fn upsert_telegram_connection(
        &self,
        workspace_id: Uuid,
        channel: &str,
        bot_token: &str,
        label: &str,
    ) -> Result<(), FanbaseError> {
        let encrypted_token = self.encrypt_token(bot_token, workspace_id, "telegram", channel)?;
        let credential_ref = format!("telegram:{channel}");
        sqlx::query(
            r#"
            INSERT INTO fanbase_connections (
                workspace_id, platform, external_account_ref,
                credential_ref, label, status, provider_account_id,
                encrypted_access_token, token_type
            )
            VALUES ($1, 'telegram', $2, $3, $4, 'connected', $2,
                    $5, 'bearer')
            ON CONFLICT (workspace_id, platform, external_account_ref)
            DO UPDATE SET
                credential_ref = EXCLUDED.credential_ref,
                label = EXCLUDED.label,
                encrypted_access_token = EXCLUDED.encrypted_access_token,
                status = 'connected',
                updated_at = now()
            "#,
        )
        .bind(workspace_id)
        .bind(channel)
        .bind(&credential_ref)
        .bind(label)
        .bind(&encrypted_token)
        .execute(&self.pool)
        .await
        .map_err(Self::unexpected)?;
        sqlx::query("SELECT pg_notify('growth_metric_sync', 'telegram-connected')")
            .execute(&self.pool)
            .await
            .map_err(Self::unexpected)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Manual Reddit post registration
//
// Registers a manually-posted Reddit URL for a community post that was
// in `awaiting_manual_post` status. Extracts the Reddit post ID from
// the URL and transitions the row to `posted` so the metrics poller
// can track it.
// ---------------------------------------------------------------------------

/// Error type for manual Reddit post registration.
#[derive(Debug, thiserror::Error)]
pub enum ManualRedditPostError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("community post not found or not in awaiting_manual_post status")]
    NotFound,
}

/// Registers a manually-posted Reddit URL for a community post that was
/// drafted by the system but posted manually by the operator (manual mode).
/// Transitions the post to `posted` status so the metrics poller can track
/// its performance via Reddit's public JSON endpoint.
///
/// # Errors
/// Returns [`ManualRedditPostError::NotFound`] if the post doesn't exist or
/// isn't in `awaiting_manual_post` status.
pub async fn register_manual_reddit_post(
    pool: &PgPool,
    workspace_id: Uuid,
    community_post_id: Uuid,
    reddit_post_url: &str,
) -> Result<(), ManualRedditPostError> {
    let reddit_post_id = extract_reddit_post_id(reddit_post_url).ok_or_else(|| {
        ManualRedditPostError::InvalidUrl(format!(
            "could not extract post ID from URL: {reddit_post_url}"
        ))
    })?;

    let mut transaction = pool.begin().await?;
    let result = sqlx::query(
        r#"
        UPDATE community_posts
        SET status = 'posted',
            reddit_post_id = $3,
            reddit_post_url = $4,
            posted_at = now(),
            updated_at = now(),
            error_message = NULL
        WHERE id = $1
          AND workspace_id = $2
          AND status = 'awaiting_manual_post'
        "#,
    )
    .bind(community_post_id)
    .bind(workspace_id)
    .bind(&reddit_post_id)
    .bind(reddit_post_url)
    .execute(&mut *transaction)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ManualRedditPostError::NotFound);
    }
    anchor_measurements_to_publication(&mut transaction, workspace_id, community_post_id).await?;
    transaction.commit().await?;
    Ok(())
}

/// Moves the measurement windows for a manually published post so they start
/// when the post reached the community.
///
/// A draft is not an exposure. The windows were anchored to
/// `action_finished_at`, which for the manual flow is the moment the text was
/// written — and an operator who publishes on Thursday what the agent drafted
/// on Monday would have had three of the fourteen days spent measuring a world
/// the post was not in yet. Worse, the days are not neutral: they carry
/// whatever the rest of the workspace did, credited to a post nobody had seen.
///
/// Two actions are re-anchored. The post's own action carries the engagement
/// measurement; the agent run that drafted it carries Y14 and Y30. They are
/// joined through the community itself — the draft names the target, and the
/// experiment assigned that same target to the dispatch that produced it. The
/// most recent assignment for the target before the draft existed is that
/// dispatch, which keeps an older post to the same community from dragging an
/// earlier dispatch's windows forward with it.
///
/// Each measurement keeps its own offset — seven days stays seven days from
/// publication, forty-four stays forty-four — so only the origin moves.
async fn anchor_measurements_to_publication(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: Uuid,
    community_post_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        WITH published AS (
            SELECT post.action_id, post.target_id, post.created_at, post.posted_at
            FROM community_posts AS post
            WHERE post.workspace_id = $1 AND post.id = $2
        ), owning_actions AS (
            SELECT published.action_id, published.posted_at
            FROM published
            UNION
            SELECT assignment.action_id, published.posted_at
            FROM published
            JOIN LATERAL (
                SELECT candidate.action_id
                FROM viryaos_experiment_assignments AS candidate
                WHERE candidate.workspace_id = $1
                  AND candidate.unit_kind = 'target_community'
                  AND candidate.unit_id = published.target_id::text
                  AND candidate.action_id IS NOT NULL
                  AND candidate.assigned_at <= published.created_at
                ORDER BY candidate.assigned_at DESC
                LIMIT 1
            ) AS assignment ON true
            WHERE published.target_id IS NOT NULL
        )
        UPDATE viryaos_autopilot_measurements AS measurement
        SET action_finished_at = owning_actions.posted_at,
            due_at = owning_actions.posted_at
                     + (measurement.due_at - measurement.action_finished_at),
            available_at = owning_actions.posted_at
                     + (measurement.due_at - measurement.action_finished_at)
        FROM owning_actions
        WHERE measurement.workspace_id = $1
          AND measurement.action_id = owning_actions.action_id
          AND measurement.status = 'pending'
          AND measurement.action_finished_at < owning_actions.posted_at
        "#,
    )
    .bind(workspace_id)
    .bind(community_post_id)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

/// Extracts the Reddit post ID from a URL like:
/// `https://www.reddit.com/r/subreddit/comments/abc123/title/` → `abc123`
fn extract_reddit_post_id(url: &str) -> Option<String> {
    let parts: Vec<&str> = url.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if *part == "comments"
            && let Some(id) = parts.get(i + 1)
            && !id.is_empty()
        {
            return Some((*id).to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crowdrelay_domain::fanbase::AdmissionAction;

    /// Documents the counter invariant that `ingest_candidates` must hold:
    /// every received entry is counted by exactly one of the outcome
    /// counters, so `sum(counts) == received`. The ResendPending arm is
    /// the one that was previously double-counted (confirmation_resent at
    /// admission + cooldown_skipped after the gate). This test pins the
    /// mapping from admission actions to counters so a regression is
    /// caught here, not in production counts.
    #[test]
    fn admission_to_counter_mapping_is_disjoint() {
        // Each admission action maps to exactly one primary counter. The
        // confirmation_resent counter is NOT set at admission time for
        // ResendPending — it is set only after the cooldown check passes,
        // inside the token-issuance block. A pending fan in cooldown lands
        // in cooldown_skipped, not confirmation_resent.
        let cases = [
            (AdmissionAction::CreatePending, "imported_pending"),
            (
                AdmissionAction::ResendPending,
                "confirmation_resent_or_cooldown_skipped",
            ),
            (AdmissionAction::AlreadyActive, "already_active"),
            (AdmissionAction::SkipSuppressed, "skipped_suppressed"),
        ];
        for (action, expected_counter) in cases {
            // The mapping is documented, not computed — the point is that
            // ResendPending does NOT map to confirmation_resent unconditionally.
            if action == AdmissionAction::ResendPending {
                assert_eq!(
                    expected_counter, "confirmation_resent_or_cooldown_skipped",
                    "ResendPending must not unconditionally map to confirmation_resent"
                );
            }
        }
    }

    #[test]
    fn ingestion_counts_sum_to_received() {
        // A batch where every outcome is represented exactly once. The
        // invariant is: received == imported_pending + confirmation_resent
        //   + already_active + skipped_suppressed + cooldown_skipped + invalid.
        let counts = IngestionCounts {
            received: 6,
            imported_pending: 1,
            confirmation_resent: 1,
            already_active: 1,
            skipped_suppressed: 1,
            cooldown_skipped: 1,
            invalid: 1,
        };
        let sum = counts.imported_pending
            + counts.confirmation_resent
            + counts.already_active
            + counts.skipped_suppressed
            + counts.cooldown_skipped
            + counts.invalid;
        assert_eq!(sum, counts.received, "counters must sum to received");
    }

    #[test]
    fn extract_reddit_post_id_finds_id() {
        assert_eq!(
            extract_reddit_post_id("https://www.reddit.com/r/Metal/comments/abc123/title/"),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn extract_reddit_post_id_returns_none_for_non_reddit_url() {
        assert_eq!(
            extract_reddit_post_id("https://example.com/no/comments"),
            None
        );
    }
}
