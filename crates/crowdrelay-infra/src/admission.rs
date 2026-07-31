//! PostgreSQL admission-pass repository.

use std::{future::Future, time::Duration};

use async_trait::async_trait;
use crowdrelay_application::{
    AdmissionRepository, ClaimAdmissionPassCommand, IssueAdmissionPassCommand,
    RedeemAdmissionPassCommand, RepositoryError, RevokeAdmissionPassCommand,
};
use crowdrelay_domain::{
    AdmissionPassClaimed, AdmissionPassId, AdmissionPassIssued, AdmissionPassStatus,
    AdmissionPassView, AdmissionRedemptionResult, AdmissionRedemptionStatus, EventId, FanId,
    PassClaimToken, PassSessionId, PassSessionToken, WorkspaceId, WorkspaceSlug,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use tokio::time::timeout;
use uuid::Uuid;

use crate::{
    config::DatabaseConfig,
    database::{SqlxErrorClass, classify_sqlx_error},
    sensitive_response::SensitiveResponseCodec,
};

const ISSUE_SCOPE: &str = "admission.pass.issue";
const CLAIM_SCOPE: &str = "admission.pass.claim";
const REDEEM_SCOPE: &str = "admission.pass.redeem";
const REVOKE_SCOPE: &str = "admission.pass.revoke";
const IDEMPOTENCY_RETENTION_DAYS: i64 = 14;
const PASS_SESSION_DAYS: i64 = 14;
const JSON_CONTENT_TYPE: &str = "application/json";
const ENCRYPTED_JSON_CONTENT_TYPE: &str = "application/vnd.crowdrelay.encrypted+json";

/// PostgreSQL implementation of the admission application port.
#[derive(Clone)]
pub struct PostgresAdmissionRepository {
    pool: PgPool,
    workspace_slug: WorkspaceSlug,
    operation_timeout: Duration,
    lock_timeout: Duration,
    admin_member_email: String,
    staff_member_email: String,
    staff_session_token_hash: [u8; 32],
    sensitive_response_codec: SensitiveResponseCodec,
}

struct AuditEventArgs<'a> {
    workspace_id: WorkspaceId,
    member_id: Uuid,
    action: &'a str,
    target_type: &'a str,
    target_id: Uuid,
    request_id: &'a str,
}

impl PostgresAdmissionRepository {
    /// Creates a tenant-scoped admission repository.
    #[must_use]
    pub fn new(
        pool: PgPool,
        workspace_slug: WorkspaceSlug,
        database: &DatabaseConfig,
        admin_member_email: String,
        staff_member_email: String,
        staff_session_token_hash: [u8; 32],
        sensitive_response_codec: SensitiveResponseCodec,
    ) -> Self {
        Self {
            pool,
            workspace_slug,
            operation_timeout: database.operation_timeout,
            lock_timeout: database.lock_timeout,
            admin_member_email,
            staff_member_email,
            staff_session_token_hash,
            sensitive_response_codec,
        }
    }

    async fn bounded<T>(
        &self,
        future: impl Future<Output = Result<T, AdmissionStoreError>>,
    ) -> Result<T, RepositoryError> {
        timeout(self.operation_timeout, future)
            .await
            .map_err(|_| RepositoryError::Unavailable)?
            .map_err(AdmissionStoreError::into_repository)
    }

    async fn issue_inner(
        &self,
        command: &IssueAdmissionPassCommand,
    ) -> Result<AdmissionPassIssued, AdmissionStoreError> {
        let request_hash = issue_request_hash(command);
        let mut transaction = self.pool.begin().await.map_err(AdmissionStoreError::sqlx)?;
        self.configure_transaction(&mut transaction).await?;
        let workspace_id = self.workspace_id(&mut transaction).await?;
        if workspace_id != command.workspace_id {
            return Err(AdmissionStoreError::NotFound);
        }
        self.lock_idempotency(
            &mut transaction,
            workspace_id,
            ISSUE_SCOPE,
            command.idempotency_key.as_str(),
        )
        .await?;
        if let Some(result) = self
            .load_sensitive_idempotent::<AdmissionPassIssued>(
                &mut transaction,
                workspace_id,
                ISSUE_SCOPE,
                command.idempotency_key.as_str(),
                &request_hash,
            )
            .await?
        {
            transaction
                .commit()
                .await
                .map_err(AdmissionStoreError::sqlx)?;
            return Ok(result);
        }

        let event_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT id
            FROM events
            WHERE workspace_id = $1
              AND slug = $2
              AND status = 'published'
              AND COALESCE(ends_at, starts_at + interval '12 hours') > now()
            FOR SHARE
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(command.event_slug.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?
        .ok_or(AdmissionStoreError::NotFound)?;

        let pool = sqlx::query_as::<_, PoolRow>(
            r#"
            SELECT id, issued_count, reserved_count, capacity
            FROM admission_pools
            WHERE workspace_id = $1 AND event_id = $2 AND slug = $3 AND active
            FOR UPDATE
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(event_id)
        .bind(command.pool_slug.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?
        .ok_or(AdmissionStoreError::NotFound)?;

        let fan = sqlx::query_as::<_, FanRow>(
            r#"
            SELECT id, normalized_email, display_name
            FROM fans
            WHERE workspace_id = $1
              AND normalized_email = $2
              AND status = 'active'
            FOR SHARE
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(command.fan_email.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?
        .ok_or(AdmissionStoreError::NotFound)?;

        let duplicate = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS(
                SELECT 1
                FROM admission_passes
                WHERE workspace_id = $1
                  AND admission_pool_id = $2
                  AND fan_id = $3
            )
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(pool.id)
        .bind(fan.id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        if duplicate || pool.issued_count.saturating_add(pool.reserved_count) >= pool.capacity {
            return Err(AdmissionStoreError::Conflict);
        }

        let admin_member_id = self
            .member_id(&mut transaction, workspace_id, &self.admin_member_email)
            .await?;
        let secret = sqlx::query_as::<_, SecretRow>(
            r#"
            SELECT
                encode(gen_random_bytes(32), 'hex') AS token,
                'VIRYA-' || upper(substr(encode(gen_random_bytes(12), 'hex'), 1, 12))
                    AS public_reference
            "#,
        )
        .fetch_one(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        let claim_token =
            PassClaimToken::parse(&secret.token).map_err(|_| AdmissionStoreError::Unexpected)?;
        let claim_expires_at = OffsetDateTime::now_utc()
            .checked_add(time::Duration::hours(i64::from(
                command.claim_expires_hours,
            )))
            .ok_or(AdmissionStoreError::Unexpected)?;

        let pass_id = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO admission_passes (
                id, workspace_id, event_id, admission_pool_id, fan_id,
                issued_by_member_id, issuance_method, public_reference,
                claim_token_hash, claim_expires_at, status
            )
            VALUES ($1, $2, $3, $4, $5, $6, 'manual', $7, digest($8, 'sha256'), $9, 'issued')
            "#,
        )
        .bind(pass_id)
        .bind(workspace_id.into_uuid())
        .bind(event_id)
        .bind(pool.id)
        .bind(fan.id)
        .bind(admin_member_id)
        .bind(&secret.public_reference)
        .bind(claim_token.as_str())
        .bind(claim_expires_at)
        .execute(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        sqlx::query(
            "UPDATE admission_pools SET issued_count = issued_count + 1 \
             WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id.into_uuid())
        .bind(pool.id)
        .execute(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;

        let result = AdmissionPassIssued {
            pass_id: AdmissionPassId::from_uuid(pass_id),
            event_id: EventId::from_uuid(event_id),
            fan_id: FanId::from_uuid(fan.id),
            public_reference: secret.public_reference.clone(),
            claim_token: claim_token.clone(),
            claim_expires_at,
            created: true,
        };
        self.append_outbox(
            &mut transaction,
            workspace_id,
            "admission.pass.issued",
            command.request_id.as_str(),
            json!({
                "pass_id": pass_id,
                "event_id": event_id,
                "fan_id": fan.id,
                "email": &fan.normalized_email,
                "display_name": &fan.display_name,
                "public_reference": &result.public_reference,
                "claim_token": claim_token.as_str(),
                "claim_expires_at": claim_expires_at,
            }),
        )
        .await?;
        self.append_audit(
            &mut transaction,
            AuditEventArgs {
                workspace_id,
                member_id: admin_member_id,
                action: "admission.pass.issued",
                target_type: "admission_pass",
                target_id: pass_id,
                request_id: command.request_id.as_str(),
            },
        )
        .await?;
        self.complete_sensitive_idempotency(
            &mut transaction,
            workspace_id,
            ISSUE_SCOPE,
            command.idempotency_key.as_str(),
            &request_hash,
            &result,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(AdmissionStoreError::sqlx)?;
        Ok(result)
    }

    async fn claim_inner(
        &self,
        command: &ClaimAdmissionPassCommand,
    ) -> Result<AdmissionPassClaimed, AdmissionStoreError> {
        let request_hash = Sha256::digest(command.token.as_str().as_bytes()).to_vec();
        let mut transaction = self.pool.begin().await.map_err(AdmissionStoreError::sqlx)?;
        self.configure_transaction(&mut transaction).await?;
        let workspace_id = self.workspace_id(&mut transaction).await?;
        if workspace_id != command.workspace_id {
            return Err(AdmissionStoreError::NotFound);
        }
        self.lock_idempotency(
            &mut transaction,
            workspace_id,
            CLAIM_SCOPE,
            command.idempotency_key.as_str(),
        )
        .await?;
        if let Some(result) = self
            .load_sensitive_idempotent::<AdmissionPassClaimed>(
                &mut transaction,
                workspace_id,
                CLAIM_SCOPE,
                command.idempotency_key.as_str(),
                &request_hash,
            )
            .await?
        {
            transaction
                .commit()
                .await
                .map_err(AdmissionStoreError::sqlx)?;
            return Ok(result);
        }
        let pass = sqlx::query_as::<_, ClaimRow>(
            r#"
            SELECT
                pass.id,
                pass.event_id,
                pass.admission_pool_id,
                pass.status,
                pass.claim_expires_at,
                GREATEST(
                    now() + ($3::bigint * interval '1 day'),
                    COALESCE(event.ends_at, event.starts_at) + interval '1 day'
                ) AS session_expires_at
            FROM admission_passes AS pass
            JOIN events AS event
                ON event.workspace_id = pass.workspace_id
                AND event.id = pass.event_id
            WHERE pass.workspace_id = $1
                AND pass.claim_token_hash = digest($2, 'sha256')
            FOR UPDATE OF pass
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(command.token.as_str())
        .bind(PASS_SESSION_DAYS)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?
        .ok_or(AdmissionStoreError::NotFound)?;
        if pass.status != "issued" {
            return Err(AdmissionStoreError::Conflict);
        }
        if pass.claim_expires_at <= OffsetDateTime::now_utc() {
            sqlx::query(
                "UPDATE admission_passes SET status = 'expired', claim_token_hash = NULL \
                 WHERE workspace_id = $1 AND id = $2",
            )
            .bind(workspace_id.into_uuid())
            .bind(pass.id)
            .execute(&mut *transaction)
            .await
            .map_err(AdmissionStoreError::sqlx)?;
            sqlx::query(
                "UPDATE admission_pools \
                 SET issued_count = GREATEST(issued_count - 1, 0) \
                 WHERE workspace_id = $1 AND id = $2",
            )
            .bind(workspace_id.into_uuid())
            .bind(pass.admission_pool_id)
            .execute(&mut *transaction)
            .await
            .map_err(AdmissionStoreError::sqlx)?;
            transaction
                .commit()
                .await
                .map_err(AdmissionStoreError::sqlx)?;
            return Err(AdmissionStoreError::Conflict);
        }

        let session_secret =
            sqlx::query_scalar::<_, String>("SELECT encode(gen_random_bytes(32), 'hex')")
                .fetch_one(&mut *transaction)
                .await
                .map_err(AdmissionStoreError::sqlx)?;
        let session_token =
            PassSessionToken::parse(session_secret).map_err(|_| AdmissionStoreError::Unexpected)?;
        let session_id = Uuid::now_v7();
        let session_expires_at = pass.session_expires_at;
        sqlx::query(
            r#"
            UPDATE admission_passes
            SET status = 'claimed', claim_token_hash = NULL, claim_token_consumed_at = now(), claimed_at = now()
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(pass.id)
        .execute(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        let session_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO pass_sessions (
                id, workspace_id, pass_id, session_token_hash, expires_at
            ) VALUES ($1, $2, $3, digest($4, 'sha256'), $5)
            ON CONFLICT (workspace_id, pass_id) DO UPDATE
            SET session_token_hash = EXCLUDED.session_token_hash,
                created_at = now(),
                last_seen_at = now(),
                expires_at = EXCLUDED.expires_at,
                revoked_at = NULL
            RETURNING id
            "#,
        )
        .bind(session_id)
        .bind(workspace_id.into_uuid())
        .bind(pass.id)
        .bind(session_token.as_str())
        .bind(session_expires_at)
        .fetch_one(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        let view = self
            .load_view_by_pass(
                &mut transaction,
                workspace_id,
                pass.id,
                Some(session_id),
                session_expires_at,
            )
            .await?;
        self.append_outbox(
            &mut transaction,
            workspace_id,
            "admission.pass.claimed",
            command.request_id.as_str(),
            json!({
                "pass_id": pass.id,
                "event_id": pass.event_id,
                "public_reference": &view.public_reference,
            }),
        )
        .await?;
        let result = AdmissionPassClaimed {
            pass: view,
            session_token,
        };
        self.complete_sensitive_idempotency(
            &mut transaction,
            workspace_id,
            CLAIM_SCOPE,
            command.idempotency_key.as_str(),
            &request_hash,
            &result,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(AdmissionStoreError::sqlx)?;
        Ok(result)
    }

    async fn load_inner(
        &self,
        workspace_id: WorkspaceId,
        session: &PassSessionToken,
    ) -> Result<AdmissionPassView, AdmissionStoreError> {
        let mut transaction = self.pool.begin().await.map_err(AdmissionStoreError::sqlx)?;
        self.configure_transaction(&mut transaction).await?;
        let trusted_workspace = self.workspace_id(&mut transaction).await?;
        if trusted_workspace != workspace_id {
            return Err(AdmissionStoreError::NotFound);
        }
        let row = sqlx::query_as::<_, SessionRow>(
            r#"
            SELECT id, pass_id, expires_at
            FROM pass_sessions
            WHERE workspace_id = $1
              AND session_token_hash = digest($2, 'sha256')
              AND revoked_at IS NULL
              AND expires_at > now()
            FOR UPDATE
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(session.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?
        .ok_or(AdmissionStoreError::NotFound)?;
        sqlx::query(
            "UPDATE pass_sessions SET last_seen_at = now() \
             WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id.into_uuid())
        .bind(row.id)
        .execute(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        let view = self
            .load_view_by_pass(
                &mut transaction,
                workspace_id,
                row.pass_id,
                Some(row.id),
                row.expires_at,
            )
            .await?;
        transaction
            .commit()
            .await
            .map_err(AdmissionStoreError::sqlx)?;
        Ok(view)
    }

    async fn redeem_inner(
        &self,
        command: &RedeemAdmissionPassCommand,
    ) -> Result<AdmissionRedemptionResult, AdmissionStoreError> {
        let request_hash = redeem_request_hash(command);
        let mut transaction = self.pool.begin().await.map_err(AdmissionStoreError::sqlx)?;
        self.configure_transaction(&mut transaction).await?;
        let workspace_id = self.workspace_id(&mut transaction).await?;
        if workspace_id != command.workspace_id {
            return Err(AdmissionStoreError::NotFound);
        }
        self.lock_idempotency(
            &mut transaction,
            workspace_id,
            REDEEM_SCOPE,
            command.idempotency_key.as_str(),
        )
        .await?;
        if let Some(result) = self
            .load_idempotent::<AdmissionRedemptionResult>(
                &mut transaction,
                workspace_id,
                REDEEM_SCOPE,
                command.idempotency_key.as_str(),
                &request_hash,
            )
            .await?
        {
            transaction
                .commit()
                .await
                .map_err(AdmissionStoreError::sqlx)?;
            return Ok(result);
        }

        let staff = self.staff_actor(&mut transaction, workspace_id).await?;
        let pass = sqlx::query_as::<_, RedemptionRow>(
            r#"
            SELECT p.id, p.event_id, p.status, p.public_reference, p.redeemed_at,
                   f.display_name, f.normalized_email
            FROM admission_passes p
            JOIN fans f ON f.workspace_id = p.workspace_id AND f.id = p.fan_id
            JOIN events e ON e.workspace_id = p.workspace_id AND e.id = p.event_id
            WHERE p.workspace_id = $1
                AND p.public_reference = $2
                AND e.slug = $3
                AND e.status = 'published'
                AND now() >= COALESCE(e.doors_at, e.starts_at) - interval '1 hour'
                AND now() <= COALESCE(e.ends_at, e.starts_at + interval '12 hours')
                    + interval '2 hours'
            FOR UPDATE OF p
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(&command.public_reference)
        .bind(command.event_slug.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?
        .ok_or(AdmissionStoreError::NotFound)?;
        if command.pass_id.is_some_and(|id| id.into_uuid() != pass.id)
            || command
                .event_id
                .is_some_and(|id| id.into_uuid() != pass.event_id)
        {
            return Err(AdmissionStoreError::Conflict);
        }

        let now = OffsetDateTime::now_utc();
        let (status, redeemed_at) = match pass.status.as_str() {
            "redeemed" => (AdmissionRedemptionStatus::AlreadyRedeemed, pass.redeemed_at),
            "revoked" => (AdmissionRedemptionStatus::Revoked, None),
            "expired" => (AdmissionRedemptionStatus::Expired, None),
            "issued" => (AdmissionRedemptionStatus::NotClaimed, None),
            "claimed" => {
                sqlx::query(
                    r#"
                    INSERT INTO pass_redemptions (
                        workspace_id, pass_id, staff_member_id, staff_session_id, request_id,
                        result_metadata
                    ) VALUES ($1, $2, $3, $4, $5, jsonb_build_object('source', 'gate_api'))
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(pass.id)
                .bind(staff.member_id)
                .bind(staff.session_id)
                .bind(command.request_id.as_str())
                .execute(&mut *transaction)
                .await
                .map_err(AdmissionStoreError::sqlx)?;
                sqlx::query(
                    "UPDATE admission_passes SET status = 'redeemed', redeemed_at = $3 \
                     WHERE workspace_id = $1 AND id = $2",
                )
                .bind(workspace_id.into_uuid())
                .bind(pass.id)
                .bind(now)
                .execute(&mut *transaction)
                .await
                .map_err(AdmissionStoreError::sqlx)?;
                self.append_outbox(
                    &mut transaction,
                    workspace_id,
                    "admission.pass.redeemed",
                    command.request_id.as_str(),
                    json!({
                        "pass_id": pass.id,
                        "event_id": pass.event_id,
                        "public_reference": &pass.public_reference,
                        "redeemed_at": now,
                    }),
                )
                .await?;
                (AdmissionRedemptionStatus::Redeemed, Some(now))
            }
            _ => return Err(AdmissionStoreError::Unexpected),
        };
        let result = AdmissionRedemptionResult {
            pass_id: AdmissionPassId::from_uuid(pass.id),
            event_id: EventId::from_uuid(pass.event_id),
            public_reference: pass.public_reference,
            holder_name: pass.display_name,
            holder_email_masked: mask_email(&pass.normalized_email),
            status,
            redeemed_at,
        };
        self.append_audit(
            &mut transaction,
            AuditEventArgs {
                workspace_id,
                member_id: staff.member_id,
                action: "admission.pass.redemption_checked",
                target_type: "admission_pass",
                target_id: pass.id,
                request_id: command.request_id.as_str(),
            },
        )
        .await?;
        self.complete_idempotency(
            &mut transaction,
            workspace_id,
            REDEEM_SCOPE,
            command.idempotency_key.as_str(),
            &request_hash,
            &result,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(AdmissionStoreError::sqlx)?;
        Ok(result)
    }

    async fn revoke_inner(
        &self,
        command: &RevokeAdmissionPassCommand,
    ) -> Result<AdmissionPassView, AdmissionStoreError> {
        let request_hash = Sha256::digest(command.public_reference.as_bytes()).to_vec();
        let mut transaction = self.pool.begin().await.map_err(AdmissionStoreError::sqlx)?;
        self.configure_transaction(&mut transaction).await?;
        let workspace_id = self.workspace_id(&mut transaction).await?;
        if workspace_id != command.workspace_id {
            return Err(AdmissionStoreError::NotFound);
        }
        self.lock_idempotency(
            &mut transaction,
            workspace_id,
            REVOKE_SCOPE,
            command.idempotency_key.as_str(),
        )
        .await?;
        if let Some(result) = self
            .load_idempotent::<AdmissionPassView>(
                &mut transaction,
                workspace_id,
                REVOKE_SCOPE,
                command.idempotency_key.as_str(),
                &request_hash,
            )
            .await?
        {
            transaction
                .commit()
                .await
                .map_err(AdmissionStoreError::sqlx)?;
            return Ok(result);
        }
        let admin_member_id = self
            .member_id(&mut transaction, workspace_id, &self.admin_member_email)
            .await?;
        let pass = sqlx::query_as::<_, RevokeRow>(
            r#"
            SELECT id, admission_pool_id, status
            FROM admission_passes
            WHERE workspace_id = $1 AND public_reference = $2
            FOR UPDATE
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(&command.public_reference)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?
        .ok_or(AdmissionStoreError::NotFound)?;
        if pass.status == "redeemed" {
            return Err(AdmissionStoreError::Conflict);
        }
        let changed = pass.status != "revoked";
        if changed {
            sqlx::query(
                "UPDATE admission_passes SET status = 'revoked', claim_token_hash = NULL \
                 WHERE workspace_id = $1 AND id = $2",
            )
            .bind(workspace_id.into_uuid())
            .bind(pass.id)
            .execute(&mut *transaction)
            .await
            .map_err(AdmissionStoreError::sqlx)?;
            sqlx::query(
                "UPDATE pass_sessions SET revoked_at = now() \
                 WHERE workspace_id = $1 AND pass_id = $2 AND revoked_at IS NULL",
            )
            .bind(workspace_id.into_uuid())
            .bind(pass.id)
            .execute(&mut *transaction)
            .await
            .map_err(AdmissionStoreError::sqlx)?;
            if releases_pool_capacity(&pass.status) {
                sqlx::query(
                    "UPDATE admission_pools \
                     SET issued_count = GREATEST(issued_count - 1, 0) \
                     WHERE workspace_id = $1 AND id = $2",
                )
                .bind(workspace_id.into_uuid())
                .bind(pass.admission_pool_id)
                .execute(&mut *transaction)
                .await
                .map_err(AdmissionStoreError::sqlx)?;
            }
        }
        let view = self
            .load_view_by_pass(
                &mut transaction,
                workspace_id,
                pass.id,
                None,
                OffsetDateTime::now_utc(),
            )
            .await?;
        if changed {
            self.append_audit(
                &mut transaction,
                AuditEventArgs {
                    workspace_id,
                    member_id: admin_member_id,
                    action: "admission.pass.revoked",
                    target_type: "admission_pass",
                    target_id: pass.id,
                    request_id: command.request_id.as_str(),
                },
            )
            .await?;
            self.append_outbox(
                &mut transaction,
                workspace_id,
                "admission.pass.revoked",
                command.request_id.as_str(),
                json!({
                    "pass_id": pass.id,
                    "public_reference": &command.public_reference,
                }),
            )
            .await?;
        }
        self.complete_idempotency(
            &mut transaction,
            workspace_id,
            REVOKE_SCOPE,
            command.idempotency_key.as_str(),
            &request_hash,
            &view,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(AdmissionStoreError::sqlx)?;
        Ok(view)
    }

    async fn configure_transaction(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
    ) -> Result<(), AdmissionStoreError> {
        let statement_ms = i64::try_from(self.operation_timeout.as_millis())
            .map_err(|_| AdmissionStoreError::Unexpected)?;
        let lock_ms = i64::try_from(self.lock_timeout.as_millis())
            .map_err(|_| AdmissionStoreError::Unexpected)?;
        sqlx::query(
            "SELECT set_config('statement_timeout', $1, true), \
             set_config('lock_timeout', $2, true)",
        )
        .bind(format!("{statement_ms}ms"))
        .bind(format!("{lock_ms}ms"))
        .execute(&mut **transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        Ok(())
    }

    async fn workspace_id(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
    ) -> Result<WorkspaceId, AdmissionStoreError> {
        let id =
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = $1 FOR SHARE")
                .bind(self.workspace_slug.as_str())
                .fetch_optional(&mut **transaction)
                .await
                .map_err(AdmissionStoreError::sqlx)?
                .ok_or(AdmissionStoreError::NotFound)?;
        Ok(WorkspaceId::from_uuid(id))
    }

    async fn member_id(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        email: &str,
    ) -> Result<Uuid, AdmissionStoreError> {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM workspace_members \
             WHERE workspace_id = $1 AND normalized_email = $2 AND status = 'active' \
             FOR SHARE",
        )
        .bind(workspace_id.into_uuid())
        .bind(email)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?
        .ok_or(AdmissionStoreError::NotFound)
    }

    async fn staff_actor(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
    ) -> Result<StaffActor, AdmissionStoreError> {
        sqlx::query_as::<_, StaffActor>(
            r#"
            SELECT m.id AS member_id, s.id AS session_id
            FROM workspace_members m
            JOIN workspace_member_sessions s
              ON s.workspace_id = m.workspace_id AND s.member_id = m.id
            WHERE m.workspace_id = $1
              AND m.normalized_email = $2
              AND m.status = 'active'
              AND s.session_token_hash = $3
              AND s.revoked_at IS NULL
              AND s.expires_at > now()
            FOR SHARE OF m, s
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(&self.staff_member_email)
        .bind(self.staff_session_token_hash.as_slice())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?
        .ok_or(AdmissionStoreError::NotFound)
    }

    async fn lock_idempotency(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        scope: &str,
        key: &str,
    ) -> Result<(), AdmissionStoreError> {
        let lock_key = format!("{}:{scope}:{key}", workspace_id);
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(lock_key)
            .execute(&mut **transaction)
            .await
            .map_err(AdmissionStoreError::sqlx)?;
        Ok(())
    }

    async fn load_idempotent<T: serde::de::DeserializeOwned>(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        scope: &str,
        key: &str,
        request_hash: &[u8],
    ) -> Result<Option<T>, AdmissionStoreError> {
        let Some(row) = self
            .load_idempotency_row(transaction, workspace_id, scope, key)
            .await?
        else {
            return Ok(None);
        };
        if row.request_hash != request_hash {
            return Err(AdmissionStoreError::Conflict);
        }
        let body = row.response_body.ok_or(AdmissionStoreError::Unexpected)?;
        serde_json::from_value(body)
            .map(Some)
            .map_err(|_| AdmissionStoreError::Unexpected)
    }

    async fn load_sensitive_idempotent<T: serde::de::DeserializeOwned + serde::Serialize>(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        scope: &str,
        key: &str,
        request_hash: &[u8],
    ) -> Result<Option<T>, AdmissionStoreError> {
        let Some(row) = self
            .load_idempotency_row(transaction, workspace_id, scope, key)
            .await?
        else {
            return Ok(None);
        };
        if row.request_hash != request_hash {
            return Err(AdmissionStoreError::Conflict);
        }
        let body = row.response_body.ok_or(AdmissionStoreError::Unexpected)?;
        let result = match row.response_content_type.as_deref() {
            Some(ENCRYPTED_JSON_CONTENT_TYPE) => self
                .sensitive_response_codec
                .decrypt(workspace_id, scope, key, body)
                .map_err(|_| AdmissionStoreError::Unexpected)?,
            Some(JSON_CONTENT_TYPE) | None => {
                serde_json::from_value(body).map_err(|_| AdmissionStoreError::Unexpected)?
            }
            Some(_) => return Err(AdmissionStoreError::Unexpected),
        };

        // Re-encrypt every successful replay with the current key. This lazily
        // migrates both legacy plaintext rows and ciphertext created with the
        // configured previous key without extending the retention window.
        let encrypted = self
            .sensitive_response_codec
            .encrypt(workspace_id, scope, key, &result)
            .map_err(|_| AdmissionStoreError::Unexpected)?;
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
        .bind(scope)
        .bind(key)
        .bind(request_hash)
        .bind(encrypted)
        .bind(ENCRYPTED_JSON_CONTENT_TYPE)
        .execute(&mut **transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        if updated.rows_affected() != 1 {
            return Err(AdmissionStoreError::Conflict);
        }
        Ok(Some(result))
    }

    async fn load_idempotency_row(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        scope: &str,
        key: &str,
    ) -> Result<Option<IdempotencyRow>, AdmissionStoreError> {
        sqlx::query(
            r#"
            DELETE FROM idempotency_keys
            WHERE workspace_id = $1 AND scope = $2 AND key = $3
              AND expires_at <= now()
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(scope)
        .bind(key)
        .execute(&mut **transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;

        sqlx::query_as::<_, IdempotencyRow>(
            r#"
            SELECT request_hash, response_body, response_content_type
            FROM idempotency_keys
            WHERE workspace_id = $1 AND scope = $2 AND key = $3
              AND state = 'completed' AND expires_at > now()
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(scope)
        .bind(key)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)
    }

    async fn complete_idempotency<T: serde::Serialize>(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        scope: &str,
        key: &str,
        request_hash: &[u8],
        result: &T,
    ) -> Result<(), AdmissionStoreError> {
        let body = serde_json::to_value(result).map_err(|_| AdmissionStoreError::Unexpected)?;
        self.complete_idempotency_body(
            transaction,
            workspace_id,
            scope,
            key,
            request_hash,
            body,
            JSON_CONTENT_TYPE,
        )
        .await
    }

    async fn complete_sensitive_idempotency<T: serde::Serialize>(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        scope: &str,
        key: &str,
        request_hash: &[u8],
        result: &T,
    ) -> Result<(), AdmissionStoreError> {
        let body = self
            .sensitive_response_codec
            .encrypt(workspace_id, scope, key, result)
            .map_err(|_| AdmissionStoreError::Unexpected)?;
        self.complete_idempotency_body(
            transaction,
            workspace_id,
            scope,
            key,
            request_hash,
            body,
            ENCRYPTED_JSON_CONTENT_TYPE,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn complete_idempotency_body(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        scope: &str,
        key: &str,
        request_hash: &[u8],
        body: Value,
        content_type: &str,
    ) -> Result<(), AdmissionStoreError> {
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
        .bind(scope)
        .bind(key)
        .bind(request_hash)
        .bind(body)
        .bind(content_type)
        .bind(IDEMPOTENCY_RETENTION_DAYS)
        .execute(&mut **transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        Ok(())
    }

    async fn load_view_by_pass(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        pass_id: Uuid,
        session_id: Option<Uuid>,
        session_expires_at: OffsetDateTime,
    ) -> Result<AdmissionPassView, AdmissionStoreError> {
        let row = sqlx::query_as::<_, PassViewRow>(
            r#"
            SELECT p.id, p.event_id, p.public_reference, p.status, p.redeemed_at,
                   e.slug AS event_slug, e.title AS event_title, e.venue, e.starts_at,
                   f.display_name, f.normalized_email
            FROM admission_passes p
            JOIN events e ON e.workspace_id = p.workspace_id AND e.id = p.event_id
            JOIN fans f ON f.workspace_id = p.workspace_id AND f.id = p.fan_id
            WHERE p.workspace_id = $1 AND p.id = $2
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(pass_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?
        .ok_or(AdmissionStoreError::NotFound)?;
        row.into_view(session_id, session_expires_at)
    }

    async fn append_outbox(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        event_type: &str,
        request_id: &str,
        payload: Value,
    ) -> Result<(), AdmissionStoreError> {
        sqlx::query(
            "INSERT INTO outbox_events \
             (workspace_id, event_type, event_version, payload, request_id) \
             VALUES ($1, $2, 1, $3, $4)",
        )
        .bind(workspace_id.into_uuid())
        .bind(event_type)
        .bind(payload)
        .bind(request_id)
        .execute(&mut **transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        Ok(())
    }

    async fn append_audit(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        args: AuditEventArgs<'_>,
    ) -> Result<(), AdmissionStoreError> {
        sqlx::query(
            "INSERT INTO audit_events \
             (workspace_id, actor_kind, actor_member_id, action, target_type, target_id, request_id) \
             VALUES ($1, 'member', $2, $3, $4, $5, $6)",
        )
        .bind(args.workspace_id.into_uuid())
        .bind(args.member_id)
        .bind(args.action)
        .bind(args.target_type)
        .bind(args.target_id.to_string())
        .bind(args.request_id)
        .execute(&mut **transaction)
        .await
        .map_err(AdmissionStoreError::sqlx)?;
        Ok(())
    }
}

#[async_trait]
impl AdmissionRepository for PostgresAdmissionRepository {
    async fn issue_pass(
        &self,
        command: &IssueAdmissionPassCommand,
    ) -> Result<AdmissionPassIssued, RepositoryError> {
        self.bounded(self.issue_inner(command)).await
    }

    async fn claim_pass(
        &self,
        command: &ClaimAdmissionPassCommand,
    ) -> Result<AdmissionPassClaimed, RepositoryError> {
        self.bounded(self.claim_inner(command)).await
    }

    async fn load_pass(
        &self,
        workspace_id: WorkspaceId,
        session: &PassSessionToken,
    ) -> Result<AdmissionPassView, RepositoryError> {
        self.bounded(self.load_inner(workspace_id, session)).await
    }

    async fn redeem_pass(
        &self,
        command: &RedeemAdmissionPassCommand,
    ) -> Result<AdmissionRedemptionResult, RepositoryError> {
        self.bounded(self.redeem_inner(command)).await
    }

    async fn revoke_pass(
        &self,
        command: &RevokeAdmissionPassCommand,
    ) -> Result<AdmissionPassView, RepositoryError> {
        self.bounded(self.revoke_inner(command)).await
    }
}

fn issue_request_hash(command: &IssueAdmissionPassCommand) -> Vec<u8> {
    let value = format!(
        "{}\0{}\0{}\0{}",
        command.event_slug,
        command.pool_slug,
        command.fan_email.as_str(),
        command.claim_expires_hours
    );
    Sha256::digest(value.as_bytes()).to_vec()
}

fn redeem_request_hash(command: &RedeemAdmissionPassCommand) -> Vec<u8> {
    let value = format!(
        "{}\0{}\0{}\0{}",
        command.event_slug,
        command.pass_id.map(|id| id.to_string()).unwrap_or_default(),
        command
            .event_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        command.public_reference,
    );
    Sha256::digest(value.as_bytes()).to_vec()
}

fn mask_email(value: &str) -> String {
    let Some((local, domain)) = value.split_once('@') else {
        return "***".to_owned();
    };
    let first = local.chars().next().unwrap_or('*');
    format!("{first}***@{domain}")
}

fn releases_pool_capacity(status: &str) -> bool {
    matches!(status, "issued" | "claimed")
}

fn parse_pass_status(value: &str) -> Result<AdmissionPassStatus, AdmissionStoreError> {
    match value {
        "issued" => Ok(AdmissionPassStatus::Issued),
        "claimed" => Ok(AdmissionPassStatus::Claimed),
        "redeemed" => Ok(AdmissionPassStatus::Redeemed),
        "revoked" => Ok(AdmissionPassStatus::Revoked),
        "expired" => Ok(AdmissionPassStatus::Expired),
        _ => Err(AdmissionStoreError::Unexpected),
    }
}

#[derive(FromRow)]
struct PoolRow {
    id: Uuid,
    issued_count: i32,
    reserved_count: i32,
    capacity: i32,
}

#[derive(FromRow)]
struct FanRow {
    id: Uuid,
    normalized_email: String,
    display_name: Option<String>,
}

#[derive(FromRow)]
struct SecretRow {
    token: String,
    public_reference: String,
}
#[derive(FromRow)]
struct ClaimRow {
    id: Uuid,
    event_id: Uuid,
    admission_pool_id: Uuid,
    status: String,
    claim_expires_at: OffsetDateTime,
    session_expires_at: OffsetDateTime,
}
#[derive(FromRow)]
struct SessionRow {
    id: Uuid,
    pass_id: Uuid,
    expires_at: OffsetDateTime,
}

#[derive(FromRow)]
struct StaffActor {
    member_id: Uuid,
    session_id: Uuid,
}

#[derive(FromRow)]
struct RevokeRow {
    id: Uuid,
    admission_pool_id: Uuid,
    status: String,
}

#[derive(FromRow)]
struct IdempotencyRow {
    request_hash: Vec<u8>,
    response_body: Option<Value>,
    response_content_type: Option<String>,
}
#[derive(FromRow)]
struct RedemptionRow {
    id: Uuid,
    event_id: Uuid,
    status: String,
    public_reference: String,
    redeemed_at: Option<OffsetDateTime>,
    display_name: Option<String>,
    normalized_email: String,
}
#[derive(FromRow)]
struct PassViewRow {
    id: Uuid,
    event_id: Uuid,
    public_reference: String,
    status: String,
    redeemed_at: Option<OffsetDateTime>,
    event_slug: String,
    event_title: String,
    venue: Option<String>,
    starts_at: OffsetDateTime,
    display_name: Option<String>,
    normalized_email: String,
}

impl PassViewRow {
    fn into_view(
        self,
        session_id: Option<Uuid>,
        session_expires_at: OffsetDateTime,
    ) -> Result<AdmissionPassView, AdmissionStoreError> {
        Ok(AdmissionPassView {
            pass_id: AdmissionPassId::from_uuid(self.id),
            session_id: session_id.map(PassSessionId::from_uuid),
            event_id: EventId::from_uuid(self.event_id),
            event_slug: self.event_slug,
            event_title: self.event_title,
            venue: self.venue,
            starts_at: self.starts_at,
            holder_name: self.display_name,
            holder_email_masked: mask_email(&self.normalized_email),
            public_reference: self.public_reference,
            status: parse_pass_status(&self.status)?,
            session_expires_at,
            redeemed_at: self.redeemed_at,
        })
    }
}

#[derive(Debug)]
enum AdmissionStoreError {
    Unavailable,
    NotFound,
    Conflict,
    Unexpected,
}

impl AdmissionStoreError {
    fn sqlx(error: sqlx::Error) -> Self {
        match classify_sqlx_error(&error) {
            SqlxErrorClass::Unavailable => Self::Unavailable,
            SqlxErrorClass::NotFound => Self::NotFound,
            SqlxErrorClass::Conflict => Self::Conflict,
            SqlxErrorClass::Unexpected => Self::Unexpected,
        }
    }

    const fn into_repository(self) -> RepositoryError {
        match self {
            Self::Unavailable => RepositoryError::Unavailable,
            Self::NotFound => RepositoryError::NotFound,
            Self::Conflict => RepositoryError::Conflict,
            Self::Unexpected => RepositoryError::Unexpected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_email_without_disclosing_local_part() {
        assert_eq!(mask_email("wojciech@example.com"), "w***@example.com");
        assert_eq!(mask_email("invalid"), "***");
    }

    #[test]
    fn parses_every_persisted_pass_state() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            parse_pass_status("claimed").map_err(|e| format!("{e:?}"))?,
            AdmissionPassStatus::Claimed
        );
        assert!(parse_pass_status("unknown").is_err());
        Ok(())
    }

    #[test]
    fn only_live_unredeemed_passes_release_pool_capacity() {
        assert!(releases_pool_capacity("issued"));
        assert!(releases_pool_capacity("claimed"));
        assert!(!releases_pool_capacity("expired"));
        assert!(!releases_pool_capacity("revoked"));
        assert!(!releases_pool_capacity("redeemed"));
    }
}
