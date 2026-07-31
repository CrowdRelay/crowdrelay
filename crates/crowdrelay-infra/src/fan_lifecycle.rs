//! PostgreSQL persistence for email confirmation and unsubscribe actions.

use std::{future::Future, time::Duration};

use async_trait::async_trait;
use crowdrelay_application::{ConfirmFanCommand, FanLifecycleRepository, RepositoryError};
use crowdrelay_domain::{
    FanActionToken, FanConfirmationResult, FanId, FanStatus, FanUnsubscribeResult, ReferralCode,
    WorkspaceId, WorkspaceSlug,
};
use serde_json::{Value, json};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use tokio::time::timeout;
use uuid::Uuid;

use crate::{
    config::DatabaseConfig,
    database::{SqlxErrorClass, classify_sqlx_error},
    referrals::{
        issue_fan_session, qualify_signup_referral_and_rewards, reverse_signup_referral_and_rewards,
    },
    sensitive_response::SensitiveResponseCodec,
};

const CONFIRMATION_TTL_DAYS: i64 = 2;
const UNSUBSCRIBE_TTL_DAYS: i64 = 730;
const CONFIRM_SCOPE: &str = "fan.confirm";
const IDEMPOTENCY_RETENTION_DAYS: i64 = 14;
const JSON_CONTENT_TYPE: &str = "application/json";
const ENCRYPTED_JSON_CONTENT_TYPE: &str = "application/vnd.crowdrelay.encrypted+json";

/// Tenant-scoped PostgreSQL repository for one-time fan lifecycle actions.
#[derive(Clone, Debug)]
pub struct PostgresFanLifecycleRepository {
    pool: PgPool,
    workspace_slug: WorkspaceSlug,
    operation_timeout: Duration,
    lock_timeout: Duration,
    sensitive_response_codec: SensitiveResponseCodec,
}

impl PostgresFanLifecycleRepository {
    /// Creates a lifecycle repository using the configured database timeouts.
    #[must_use]
    pub fn new(
        pool: PgPool,
        workspace_slug: WorkspaceSlug,
        database: &DatabaseConfig,
        sensitive_response_codec: SensitiveResponseCodec,
    ) -> Self {
        Self {
            pool,
            workspace_slug,
            operation_timeout: database.operation_timeout,
            lock_timeout: database.lock_timeout,
            sensitive_response_codec,
        }
    }

    async fn bounded<T>(
        &self,
        operation: impl Future<Output = Result<T, LifecycleStoreError>>,
    ) -> Result<T, LifecycleStoreError> {
        timeout(self.operation_timeout, operation)
            .await
            .map_err(|_| LifecycleStoreError::Unavailable)?
    }

    async fn confirm_inner(
        &self,
        command: &ConfirmFanCommand,
    ) -> Result<FanConfirmationResult, LifecycleStoreError> {
        let workspace_id = command.workspace_id;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(LifecycleStoreError::from_sqlx)?;
        configure_transaction(&mut transaction, self.operation_timeout, self.lock_timeout).await?;
        self.verify_workspace(&mut transaction, workspace_id)
            .await?;
        let request_hash = confirmation_request_hash(command);
        self.lock_idempotency(
            &mut transaction,
            workspace_id,
            command.idempotency_key.as_str(),
        )
        .await?;
        if let Some(result) = self
            .load_confirmation_replay(
                &mut transaction,
                workspace_id,
                command.idempotency_key.as_str(),
                request_hash.as_slice(),
            )
            .await?
        {
            transaction
                .commit()
                .await
                .map_err(LifecycleStoreError::from_sqlx)?;
            return Ok(result);
        }

        let token_hash = sha256_bytes(command.token.as_str());
        let row = sqlx::query_as::<_, LifecycleTokenRow>(
            r#"
            SELECT
                fan_action_tokens.id AS token_id,
                fan_action_tokens.fan_id,
                fan_action_tokens.purpose,
                fan_action_tokens.expires_at,
                fan_action_tokens.consumed_at,
                fans.status,
                fans.normalized_email,
                fans.display_name
            FROM fan_action_tokens
            INNER JOIN fans
                ON fans.workspace_id = fan_action_tokens.workspace_id
                AND fans.id = fan_action_tokens.fan_id
            WHERE fan_action_tokens.workspace_id = $1
                AND fan_action_tokens.token_hash = $2
            FOR UPDATE OF fan_action_tokens, fans
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(token_hash.as_slice())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(LifecycleStoreError::from_sqlx)?
        .ok_or(LifecycleStoreError::NotFound)?;

        if row.consumed_at.is_some() || row.expires_at <= OffsetDateTime::now_utc() {
            return Err(LifecycleStoreError::Conflict);
        }
        let previous_status = parse_fan_status(&row.status)?;
        let confirms_pending_signup =
            row.purpose == "confirm" && previous_status == FanStatus::Pending;
        let recovers_active_session =
            row.purpose == "session" && previous_status == FanStatus::Active;
        // Each action token is bound to one exact lifecycle state. A stale
        // confirmation must never reactivate an unsubscribed fan, and a
        // recovery token must never create a session after consent withdrawal.
        if !confirms_pending_signup && !recovers_active_session {
            return Err(LifecycleStoreError::Conflict);
        }

        let fan_id = FanId::from_uuid(row.fan_id);
        sqlx::query(
            r#"
            UPDATE fan_action_tokens
            SET consumed_at = now()
            WHERE workspace_id = $1 AND id = $2 AND consumed_at IS NULL
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(row.token_id)
        .execute(&mut *transaction)
        .await
        .map_err(LifecycleStoreError::from_sqlx)?;

        if recovers_active_session {
            let referral_code =
                load_or_create_referral_code(&mut transaction, workspace_id, fan_id).await?;
            let fan_session_token = issue_fan_session(&mut transaction, workspace_id, fan_id)
                .await
                .map_err(map_referral_error)?;
            append_outbox(
                &mut transaction,
                workspace_id,
                "fan.session_recovered",
                command.request_id.as_str(),
                json!({
                    "workspace_id": workspace_id,
                    "fan_id": fan_id,
                }),
            )
            .await?;
            let result = FanConfirmationResult {
                fan_id,
                status: FanStatus::Active,
                referral_code,
                fan_session_token,
            };
            self.complete_confirmation_idempotency(
                &mut transaction,
                workspace_id,
                command.idempotency_key.as_str(),
                request_hash.as_slice(),
                &result,
            )
            .await?;
            transaction
                .commit()
                .await
                .map_err(LifecycleStoreError::from_sqlx)?;
            return Ok(result);
        }

        sqlx::query(
            r#"
            UPDATE fans
            SET status = 'active'
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(row.fan_id)
        .execute(&mut *transaction)
        .await
        .map_err(LifecycleStoreError::from_sqlx)?;

        increment_city_aggregates(&mut transaction, workspace_id, fan_id).await?;

        let attribution = sqlx::query_as::<_, AttributionRow>(
            r#"
            WITH latest_acquisition AS (
                SELECT referral_code_id, referrer_fan_id, request_id
                FROM fan_acquisition_events
                WHERE workspace_id = $1 AND fan_id = $2
                ORDER BY occurred_at DESC, id DESC
                LIMIT 1
            )
            SELECT
                COALESCE(attribution.referral_code_id, acquisition.referral_code_id)
                    AS referral_code_id,
                COALESCE(attribution.referrer_fan_id, acquisition.referrer_fan_id)
                    AS referrer_fan_id,
                acquisition.request_id
            FROM latest_acquisition AS acquisition
            LEFT JOIN referral_attributions AS attribution
                ON attribution.workspace_id = $1
                AND attribution.referred_fan_id = $2
                AND attribution.status = 'pending'
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(row.fan_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(LifecycleStoreError::from_sqlx)?;
        let attribution_request_id = attribution
            .as_ref()
            .map(|value| value.request_id.as_str())
            .unwrap_or(command.request_id.as_str());
        qualify_signup_referral_and_rewards(
            &mut transaction,
            workspace_id,
            fan_id,
            attribution
                .as_ref()
                .and_then(|value| value.referral_code_id),
            attribution.as_ref().and_then(|value| value.referrer_fan_id),
            attribution_request_id,
        )
        .await
        .map_err(map_referral_error)?;

        let referral_code =
            load_or_create_referral_code(&mut transaction, workspace_id, fan_id).await?;
        let fan_session_token = issue_fan_session(&mut transaction, workspace_id, fan_id)
            .await
            .map_err(map_referral_error)?;
        let unsubscribe_token = issue_fan_action_token(
            &mut transaction,
            workspace_id,
            fan_id,
            "unsubscribe",
            UNSUBSCRIBE_TTL_DAYS,
        )
        .await?;

        append_outbox(
            &mut transaction,
            workspace_id,
            "fan.confirmed",
            command.request_id.as_str(),
            json!({
                "workspace_id": workspace_id,
                "fan_id": fan_id,
                "email": row.normalized_email,
                "display_name": row.display_name,
                "referral_code": referral_code,
                "unsubscribe_token": unsubscribe_token.as_str(),
            }),
        )
        .await?;

        let result = FanConfirmationResult {
            fan_id,
            status: FanStatus::Active,
            referral_code,
            fan_session_token,
        };
        self.complete_confirmation_idempotency(
            &mut transaction,
            workspace_id,
            command.idempotency_key.as_str(),
            request_hash.as_slice(),
            &result,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(LifecycleStoreError::from_sqlx)?;
        Ok(result)
    }

    async fn lock_idempotency(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        key: &str,
    ) -> Result<(), LifecycleStoreError> {
        let lock_key = format!("{}:{CONFIRM_SCOPE}:{key}", workspace_id);
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(lock_key)
            .execute(&mut **transaction)
            .await
            .map_err(LifecycleStoreError::from_sqlx)?;
        Ok(())
    }

    async fn load_confirmation_replay(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        key: &str,
        request_hash: &[u8],
    ) -> Result<Option<FanConfirmationResult>, LifecycleStoreError> {
        sqlx::query(
            r#"
            DELETE FROM idempotency_keys
            WHERE workspace_id = $1 AND scope = $2 AND key = $3
              AND expires_at <= now()
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(CONFIRM_SCOPE)
        .bind(key)
        .execute(&mut **transaction)
        .await
        .map_err(LifecycleStoreError::from_sqlx)?;

        let row = sqlx::query_as::<_, IdempotencyRow>(
            r#"
            SELECT request_hash, state, response_body, response_content_type
            FROM idempotency_keys
            WHERE workspace_id = $1 AND scope = $2 AND key = $3
              AND expires_at > now()
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(CONFIRM_SCOPE)
        .bind(key)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(LifecycleStoreError::from_sqlx)?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.request_hash.as_slice() != request_hash {
            return Err(LifecycleStoreError::Conflict);
        }
        if row.state != "completed" {
            return Err(LifecycleStoreError::Conflict);
        }
        let body = row.response_body.ok_or(LifecycleStoreError::Unexpected)?;
        let result = match row.response_content_type.as_deref() {
            Some(ENCRYPTED_JSON_CONTENT_TYPE) => self
                .sensitive_response_codec
                .decrypt(workspace_id, CONFIRM_SCOPE, key, body)
                .map_err(|_| LifecycleStoreError::Unexpected)?,
            Some(JSON_CONTENT_TYPE) | None => {
                serde_json::from_value(body).map_err(|_| LifecycleStoreError::Unexpected)?
            }
            Some(_) => return Err(LifecycleStoreError::Unexpected),
        };
        let encrypted = self
            .sensitive_response_codec
            .encrypt(workspace_id, CONFIRM_SCOPE, key, &result)
            .map_err(|_| LifecycleStoreError::Unexpected)?;
        let updated = sqlx::query(
            r#"
            UPDATE idempotency_keys
            SET response_body = $5, response_content_type = $6
            WHERE workspace_id = $1 AND scope = $2 AND key = $3
              AND request_hash = $4
              AND state = 'completed' AND expires_at > now()
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(CONFIRM_SCOPE)
        .bind(key)
        .bind(request_hash)
        .bind(encrypted)
        .bind(ENCRYPTED_JSON_CONTENT_TYPE)
        .execute(&mut **transaction)
        .await
        .map_err(LifecycleStoreError::from_sqlx)?;
        if updated.rows_affected() != 1 {
            return Err(LifecycleStoreError::Conflict);
        }
        Ok(Some(result))
    }

    async fn complete_confirmation_idempotency(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        key: &str,
        request_hash: &[u8],
        result: &FanConfirmationResult,
    ) -> Result<(), LifecycleStoreError> {
        let body = self
            .sensitive_response_codec
            .encrypt(workspace_id, CONFIRM_SCOPE, key, result)
            .map_err(|_| LifecycleStoreError::Unexpected)?;
        sqlx::query(
            r#"
            INSERT INTO idempotency_keys (
                workspace_id, scope, key, request_hash, state, response_status,
                response_body, response_content_type, completed_at, expires_at
            ) VALUES (
                $1, $2, $3, $4, 'completed', 200, $5, $6,
                now(), now() + ($7::bigint * interval '1 day')
            )
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(CONFIRM_SCOPE)
        .bind(key)
        .bind(request_hash)
        .bind(body)
        .bind(ENCRYPTED_JSON_CONTENT_TYPE)
        .bind(IDEMPOTENCY_RETENTION_DAYS)
        .execute(&mut **transaction)
        .await
        .map_err(LifecycleStoreError::from_sqlx)?;
        Ok(())
    }

    async fn unsubscribe_inner(
        &self,
        workspace_id: WorkspaceId,
        token: &FanActionToken,
    ) -> Result<FanUnsubscribeResult, LifecycleStoreError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(LifecycleStoreError::from_sqlx)?;
        configure_transaction(&mut transaction, self.operation_timeout, self.lock_timeout).await?;
        self.verify_workspace(&mut transaction, workspace_id)
            .await?;

        let token_hash = sha256_bytes(token.as_str());
        let row = sqlx::query_as::<_, LifecycleTokenRow>(
            r#"
            SELECT
                fan_action_tokens.id AS token_id,
                fan_action_tokens.fan_id,
                fan_action_tokens.purpose,
                fan_action_tokens.expires_at,
                fan_action_tokens.consumed_at,
                fans.status,
                fans.normalized_email,
                fans.display_name
            FROM fan_action_tokens
            INNER JOIN fans
                ON fans.workspace_id = fan_action_tokens.workspace_id
                AND fans.id = fan_action_tokens.fan_id
            WHERE fan_action_tokens.workspace_id = $1
                AND fan_action_tokens.token_hash = $2
            FOR UPDATE OF fan_action_tokens, fans
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(token_hash.as_slice())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(LifecycleStoreError::from_sqlx)?
        .ok_or(LifecycleStoreError::NotFound)?;

        if row.purpose != "unsubscribe" || row.expires_at <= OffsetDateTime::now_utc() {
            return Err(LifecycleStoreError::Conflict);
        }
        let previous_status = parse_fan_status(&row.status)?;
        let fan_id = FanId::from_uuid(row.fan_id);
        if row.consumed_at.is_none() {
            if previous_status == FanStatus::Active {
                decrement_city_aggregates(&mut transaction, workspace_id, fan_id).await?;
            }
            sqlx::query(
                r#"
                UPDATE fans
                SET status = 'unsubscribed'
                WHERE workspace_id = $1 AND id = $2 AND status <> 'suppressed'
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(row.fan_id)
            .execute(&mut *transaction)
            .await
            .map_err(LifecycleStoreError::from_sqlx)?;
            sqlx::query(
                r#"
                INSERT INTO fan_consents (
                    workspace_id, fan_id, purpose, granted, policy_version, source, request_id
                )
                VALUES ($1, $2, 'marketing', false, 'unsubscribe-v1', 'unsubscribe-link', $3)
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(row.fan_id)
            .bind(format!("unsubscribe:{}", row.token_id))
            .execute(&mut *transaction)
            .await
            .map_err(LifecycleStoreError::from_sqlx)?;
            sqlx::query(
                r#"
                UPDATE fan_sessions
                SET revoked_at = COALESCE(revoked_at, now())
                WHERE workspace_id = $1 AND fan_id = $2 AND revoked_at IS NULL
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(row.fan_id)
            .execute(&mut *transaction)
            .await
            .map_err(LifecycleStoreError::from_sqlx)?;
            sqlx::query(
                r#"
                UPDATE event_reminder_jobs
                SET status = 'cancelled', cancelled_at = now()
                WHERE workspace_id = $1
                    AND fan_id = $2
                    AND status = 'pending'
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(row.fan_id)
            .execute(&mut *transaction)
            .await
            .map_err(LifecycleStoreError::from_sqlx)?;
            reverse_signup_referral_and_rewards(
                &mut transaction,
                workspace_id,
                fan_id,
                &format!("unsubscribe:{}", row.token_id),
            )
            .await
            .map_err(map_referral_error)?;
            sqlx::query(
                r#"
                UPDATE fan_action_tokens
                SET consumed_at = COALESCE(consumed_at, now())
                WHERE workspace_id = $1
                    AND fan_id = $2
                    AND consumed_at IS NULL
                    AND (id = $3 OR purpose IN ('confirm', 'session'))
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(row.fan_id)
            .bind(row.token_id)
            .execute(&mut *transaction)
            .await
            .map_err(LifecycleStoreError::from_sqlx)?;
            append_outbox(
                &mut transaction,
                workspace_id,
                "fan.unsubscribed",
                &format!("unsubscribe:{}", row.token_id),
                json!({
                    "workspace_id": workspace_id,
                    "fan_id": fan_id,
                    "email": row.normalized_email,
                }),
            )
            .await?;
        }

        transaction
            .commit()
            .await
            .map_err(LifecycleStoreError::from_sqlx)?;
        Ok(FanUnsubscribeResult {
            fan_id,
            status: if previous_status == FanStatus::Suppressed {
                FanStatus::Suppressed
            } else {
                FanStatus::Unsubscribed
            },
        })
    }

    async fn verify_workspace(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
    ) -> Result<(), LifecycleStoreError> {
        let trusted =
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = $1 FOR SHARE")
                .bind(self.workspace_slug.as_str())
                .fetch_optional(&mut **transaction)
                .await
                .map_err(LifecycleStoreError::from_sqlx)?
                .ok_or(LifecycleStoreError::NotFound)?;
        if trusted != workspace_id.into_uuid() {
            return Err(LifecycleStoreError::NotFound);
        }
        Ok(())
    }
}

#[async_trait]
impl FanLifecycleRepository for PostgresFanLifecycleRepository {
    async fn confirm(
        &self,
        command: &ConfirmFanCommand,
    ) -> Result<FanConfirmationResult, RepositoryError> {
        self.bounded(self.confirm_inner(command))
            .await
            .map_err(Into::into)
    }

    async fn unsubscribe(
        &self,
        workspace_id: WorkspaceId,
        token: &FanActionToken,
    ) -> Result<FanUnsubscribeResult, RepositoryError> {
        self.bounded(self.unsubscribe_inner(workspace_id, token))
            .await
            .map_err(Into::into)
    }
}

/// Creates a new one-time action token and consumes any previous live token of the same purpose.
pub(crate) async fn issue_fan_action_token(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    fan_id: FanId,
    purpose: &str,
    ttl_days: i64,
) -> Result<FanActionToken, LifecycleStoreError> {
    sqlx::query(
        r#"
        UPDATE fan_action_tokens
        SET consumed_at = COALESCE(consumed_at, now())
        WHERE workspace_id = $1
            AND fan_id = $2
            AND purpose = $3
            AND consumed_at IS NULL
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id.into_uuid())
    .bind(purpose)
    .execute(&mut **transaction)
    .await
    .map_err(LifecycleStoreError::from_sqlx)?;

    let raw = sqlx::query_scalar::<_, String>(
        r#"
        WITH material AS (
            SELECT encode(gen_random_bytes(32), 'hex') AS token
        ), inserted AS (
            INSERT INTO fan_action_tokens (
                workspace_id, fan_id, purpose, token_hash, expires_at
            )
            SELECT $1, $2, $3, digest(material.token, 'sha256'),
                now() + ($4::bigint * interval '1 day')
            FROM material
            RETURNING id
        )
        SELECT material.token
        FROM material, inserted
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id.into_uuid())
    .bind(purpose)
    .bind(ttl_days)
    .fetch_one(&mut **transaction)
    .await
    .map_err(LifecycleStoreError::from_sqlx)?;
    FanActionToken::parse(raw).map_err(|_| LifecycleStoreError::Unexpected)
}

/// Creates a short-lived inbox confirmation token.
pub(crate) async fn issue_confirmation_token(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    fan_id: FanId,
) -> Result<FanActionToken, LifecycleStoreError> {
    issue_fan_action_token(
        transaction,
        workspace_id,
        fan_id,
        "confirm",
        CONFIRMATION_TTL_DAYS,
    )
    .await
}

async fn load_or_create_referral_code(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    fan_id: FanId,
) -> Result<ReferralCode, LifecycleStoreError> {
    let code = sqlx::query_scalar::<_, String>(
        r#"
        WITH existing AS (
            SELECT code
            FROM referral_codes
            WHERE workspace_id = $1 AND fan_id = $2 AND active
            ORDER BY created_at, id
            LIMIT 1
        ), inserted AS (
            INSERT INTO referral_codes (workspace_id, fan_id, code)
            SELECT $1, $2, encode(gen_random_bytes(18), 'hex')
            WHERE NOT EXISTS (SELECT 1 FROM existing)
            ON CONFLICT DO NOTHING
            RETURNING code
        )
        SELECT code FROM existing
        UNION ALL
        SELECT code FROM inserted
        LIMIT 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(LifecycleStoreError::from_sqlx)?
    .ok_or(LifecycleStoreError::Unavailable)?;
    ReferralCode::parse(code).map_err(|_| LifecycleStoreError::Unexpected)
}

async fn increment_city_aggregates(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    fan_id: FanId,
) -> Result<(), LifecycleStoreError> {
    sqlx::query(
        r#"
        INSERT INTO city_aggregates (workspace_id, city_id, confirmed_fan_count)
        SELECT workspace_id, city_id, 1
        FROM fan_city_interests
        WHERE workspace_id = $1 AND fan_id = $2
        ON CONFLICT (workspace_id, city_id) DO UPDATE
        SET confirmed_fan_count = city_aggregates.confirmed_fan_count + 1,
            updated_at = now()
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id.into_uuid())
    .execute(&mut **transaction)
    .await
    .map_err(LifecycleStoreError::from_sqlx)?;
    Ok(())
}

async fn decrement_city_aggregates(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    fan_id: FanId,
) -> Result<(), LifecycleStoreError> {
    sqlx::query(
        r#"
        UPDATE city_aggregates AS aggregates
        SET confirmed_fan_count = GREATEST(aggregates.confirmed_fan_count - 1, 0),
            updated_at = now()
        FROM fan_city_interests AS interests
        WHERE interests.workspace_id = $1
            AND interests.fan_id = $2
            AND aggregates.workspace_id = interests.workspace_id
            AND aggregates.city_id = interests.city_id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id.into_uuid())
    .execute(&mut **transaction)
    .await
    .map_err(LifecycleStoreError::from_sqlx)?;
    Ok(())
}

async fn configure_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    operation_timeout: Duration,
    lock_timeout: Duration,
) -> Result<(), LifecycleStoreError> {
    sqlx::query(
        r#"
        SELECT
            set_config('statement_timeout', $1, true),
            set_config('lock_timeout', $2, true)
        "#,
    )
    .bind(format!("{}ms", duration_millis(operation_timeout)?))
    .bind(format!("{}ms", duration_millis(lock_timeout)?))
    .execute(&mut **transaction)
    .await
    .map_err(LifecycleStoreError::from_sqlx)?;
    Ok(())
}

async fn append_outbox(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    event_type: &str,
    request_id: &str,
    payload: serde_json::Value,
) -> Result<(), LifecycleStoreError> {
    sqlx::query(
        r#"
        INSERT INTO outbox_events (
            workspace_id, event_type, event_version, payload, request_id
        )
        VALUES ($1, $2, 1, $3, $4)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event_type)
    .bind(payload)
    .bind(request_id)
    .execute(&mut **transaction)
    .await
    .map_err(LifecycleStoreError::from_sqlx)?;
    Ok(())
}

fn sha256_bytes(value: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(value.as_bytes()).into()
}

fn confirmation_request_hash(command: &ConfirmFanCommand) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    let mut digest = Sha256::new();
    digest.update(b"crowdrelay:fan-confirm:v1\0");
    digest.update(command.workspace_id.into_uuid().as_bytes());
    digest.update(sha256_bytes(command.token.as_str()));
    digest.finalize().into()
}

fn duration_millis(value: Duration) -> Result<i64, LifecycleStoreError> {
    i64::try_from(value.as_millis()).map_err(|_| LifecycleStoreError::Unexpected)
}

fn parse_fan_status(value: &str) -> Result<FanStatus, LifecycleStoreError> {
    match value {
        "pending" => Ok(FanStatus::Pending),
        "active" => Ok(FanStatus::Active),
        "unsubscribed" => Ok(FanStatus::Unsubscribed),
        "suppressed" => Ok(FanStatus::Suppressed),
        _ => Err(LifecycleStoreError::Unexpected),
    }
}

fn map_referral_error(error: crate::referrals::ReferralStoreError) -> LifecycleStoreError {
    match error {
        crate::referrals::ReferralStoreError::Unavailable => LifecycleStoreError::Unavailable,
        crate::referrals::ReferralStoreError::NotFound => LifecycleStoreError::NotFound,
        crate::referrals::ReferralStoreError::Conflict => LifecycleStoreError::Conflict,
        crate::referrals::ReferralStoreError::Unexpected => LifecycleStoreError::Unexpected,
    }
}

#[derive(Debug, FromRow)]
struct LifecycleTokenRow {
    token_id: Uuid,
    fan_id: Uuid,
    purpose: String,
    expires_at: OffsetDateTime,
    consumed_at: Option<OffsetDateTime>,
    status: String,
    normalized_email: String,
    display_name: Option<String>,
}

#[derive(Debug, FromRow)]
struct AttributionRow {
    referral_code_id: Option<Uuid>,
    referrer_fan_id: Option<Uuid>,
    request_id: String,
}

#[derive(Debug, FromRow)]
struct IdempotencyRow {
    request_hash: Vec<u8>,
    state: String,
    response_body: Option<Value>,
    response_content_type: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum LifecycleStoreError {
    #[error("lifecycle store is unavailable")]
    Unavailable,
    #[error("lifecycle resource was not found")]
    NotFound,
    #[error("lifecycle state conflicts with the requested operation")]
    Conflict,
    #[error("lifecycle store returned an unexpected value")]
    Unexpected,
}

impl LifecycleStoreError {
    fn from_sqlx(error: sqlx::Error) -> Self {
        let class = classify_sqlx_error(&error);
        match class {
            SqlxErrorClass::Unavailable => Self::Unavailable,
            SqlxErrorClass::NotFound => Self::NotFound,
            SqlxErrorClass::Conflict => Self::Conflict,
            SqlxErrorClass::Unexpected => Self::Unexpected,
        }
    }
}

impl From<LifecycleStoreError> for RepositoryError {
    fn from(value: LifecycleStoreError) -> Self {
        match value {
            LifecycleStoreError::Unavailable => Self::Unavailable,
            LifecycleStoreError::NotFound => Self::NotFound,
            LifecycleStoreError::Conflict => Self::Conflict,
            LifecycleStoreError::Unexpected => Self::Unexpected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parser_rejects_unknown_database_values() {
        assert!(matches!(parse_fan_status("active"), Ok(FanStatus::Active)));
        assert!(parse_fan_status("deleted").is_err());
    }

    #[test]
    fn token_hash_is_stable_and_not_the_token() {
        let token = "a".repeat(64);
        let hash = sha256_bytes(&token);
        assert_eq!(hash, sha256_bytes(&token));
        assert_ne!(hash.as_slice(), token.as_bytes());
    }
}
