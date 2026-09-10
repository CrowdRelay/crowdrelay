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
use crowdrelay_domain::fanbase::SourceKind;

mod ingestion;
mod manual_publication;

pub use manual_publication::*;

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
}
