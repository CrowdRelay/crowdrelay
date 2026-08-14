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
}

include!("admission/issue_claim.rs");
include!("admission/pass_lifecycle.rs");
include!("admission/support.rs");

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
