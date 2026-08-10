//! PostgreSQL referral qualification, deterministic merch rewards, fan progress, and redemption.

use std::{future::Future, time::Duration};

use async_trait::async_trait;
use crowdrelay_application::{RedeemCouponCommand, ReferralRepository, RepositoryError};
use crowdrelay_domain::{
    CouponCode, CouponRedemptionResult, CouponStatus, FanId, FanSessionToken, MerchCoupon,
    MerchCouponId, PhysicalRewardGrant, PhysicalRewardStatus, ReferralCode, ReferralProgress,
    RewardDrawId, RewardDrawPrizeKind, RewardGrantId, RewardRuleId, WeightedDrawEntry, WorkspaceId,
    WorkspaceSlug,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use tokio::time::timeout;
use uuid::Uuid;

use crate::{
    config::DatabaseConfig,
    database::{SqlxErrorClass, classify_sqlx_error},
};

const REDEEM_IDEMPOTENCY_SCOPE: &str = "coupon_redeem";
const IDEMPOTENCY_RETENTION_MILLISECONDS: i64 = 86_400_000;
const FAN_SESSION_TTL_DAYS: i64 = 90;

/// Tenant-scoped PostgreSQL referral repository.
#[derive(Clone, Debug)]
pub struct PostgresReferralRepository {
    pool: PgPool,
    workspace_slug: WorkspaceSlug,
    operation_timeout: Duration,
    lock_timeout: Duration,
}

include!("referrals/repository.rs");
#[async_trait]
impl ReferralRepository for PostgresReferralRepository {
    async fn referral_code_is_active(
        &self,
        workspace_id: WorkspaceId,
        code: &ReferralCode,
    ) -> Result<bool, RepositoryError> {
        self.bounded(self.referral_code_is_active_inner(workspace_id, code))
            .await
            .map_err(Into::into)
    }

    async fn load_referral_progress(
        &self,
        workspace_id: WorkspaceId,
        session_token: &FanSessionToken,
    ) -> Result<ReferralProgress, RepositoryError> {
        self.bounded(self.load_referral_progress_inner(workspace_id, session_token))
            .await
            .map_err(Into::into)
    }

    async fn redeem_coupon(
        &self,
        command: &RedeemCouponCommand,
    ) -> Result<CouponRedemptionResult, RepositoryError> {
        self.bounded(self.redeem_coupon_inner(command))
            .await
            .map_err(Into::into)
    }
}

include!("referrals/reward_lifecycle.rs");
async fn configure_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    operation_timeout: Duration,
    lock_timeout: Duration,
) -> Result<(), ReferralStoreError> {
    let statement_timeout = duration_as_milliseconds(operation_timeout)?;
    let lock_timeout = duration_as_milliseconds(lock_timeout)?;
    sqlx::query(
        r#"
        SELECT
            set_config('statement_timeout', $1, true),
            set_config('lock_timeout', $2, true)
        "#,
    )
    .bind(format!("{statement_timeout}ms"))
    .bind(format!("{lock_timeout}ms"))
    .execute(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;
    Ok(())
}

async fn trusted_workspace_id_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_slug: &WorkspaceSlug,
) -> Result<WorkspaceId, ReferralStoreError> {
    let id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = $1 FOR SHARE")
        .bind(workspace_slug.as_str())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(ReferralStoreError::from_sqlx)?
        .ok_or(ReferralStoreError::NotFound)?;
    Ok(WorkspaceId::from_uuid(id))
}

async fn append_outbox(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    event_type: &str,
    request_id: &str,
    payload: serde_json::Value,
) -> Result<(), ReferralStoreError> {
    sqlx::query(
        r#"
        INSERT INTO outbox_events (
            workspace_id,
            event_type,
            event_version,
            payload,
            request_id
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
    .map_err(ReferralStoreError::from_sqlx)?;
    Ok(())
}

async fn start_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    command: &RedeemCouponCommand,
    request_hash: &[u8],
    operation_timeout: Duration,
) -> Result<bool, ReferralStoreError> {
    let lease_ms = duration_as_milliseconds(operation_timeout)?;
    sqlx::query(
        r#"
        DELETE FROM idempotency_keys
        WHERE workspace_id = $1
            AND scope = $2
            AND key = $3
            AND expires_at <= now()
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(REDEEM_IDEMPOTENCY_SCOPE)
    .bind(command.idempotency_key().as_str())
    .execute(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;

    let result = sqlx::query(
        r#"
        INSERT INTO idempotency_keys (
            workspace_id, scope, key, request_hash, state,
            lease_owner, lease_expires_at, expires_at
        )
        VALUES (
            $1, $2, $3, $4, 'in_progress', $5,
            now() + ($6::bigint * interval '1 millisecond'),
            now() + ($7::bigint * interval '1 millisecond')
        )
        ON CONFLICT (workspace_id, scope, key) DO NOTHING
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(REDEEM_IDEMPOTENCY_SCOPE)
    .bind(command.idempotency_key().as_str())
    .bind(request_hash)
    .bind(command.request_id().as_str())
    .bind(lease_ms)
    .bind(IDEMPOTENCY_RETENTION_MILLISECONDS)
    .execute(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;
    Ok(result.rows_affected() == 1)
}

async fn lock_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    command: &RedeemCouponCommand,
) -> Result<IdempotencyRow, ReferralStoreError> {
    sqlx::query_as::<_, IdempotencyRow>(
        r#"
        SELECT
            request_hash,
            state,
            response_body::text AS response_body,
            coalesce(lease_expires_at <= now(), false) AS lease_expired
        FROM idempotency_keys
        WHERE workspace_id = $1 AND scope = $2 AND key = $3
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(REDEEM_IDEMPOTENCY_SCOPE)
    .bind(command.idempotency_key().as_str())
    .fetch_one(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)
}

async fn reclaim_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    command: &RedeemCouponCommand,
    operation_timeout: Duration,
) -> Result<(), ReferralStoreError> {
    let lease_ms = duration_as_milliseconds(operation_timeout)?;
    let result = sqlx::query(
        r#"
        UPDATE idempotency_keys
        SET
            lease_owner = $4,
            lease_expires_at = now() + ($5::bigint * interval '1 millisecond')
        WHERE workspace_id = $1 AND scope = $2 AND key = $3
            AND state = 'in_progress'
            AND lease_expires_at <= now()
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(REDEEM_IDEMPOTENCY_SCOPE)
    .bind(command.idempotency_key().as_str())
    .bind(command.request_id().as_str())
    .bind(lease_ms)
    .execute(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;
    if result.rows_affected() != 1 {
        return Err(ReferralStoreError::Conflict);
    }
    Ok(())
}

async fn complete_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    command: &RedeemCouponCommand,
    request_hash: &[u8],
    result: &CouponRedemptionResult,
) -> Result<(), ReferralStoreError> {
    let response = serde_json::to_value(result).map_err(|_| ReferralStoreError::Unexpected)?;
    let updated = sqlx::query(
        r#"
        UPDATE idempotency_keys
        SET
            state = 'completed',
            lease_owner = NULL,
            lease_expires_at = NULL,
            response_status = 200,
            response_body = $5,
            response_content_type = 'application/json',
            completed_at = now()
        WHERE workspace_id = $1 AND scope = $2 AND key = $3
            AND request_hash = $4 AND state = 'in_progress'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(REDEEM_IDEMPOTENCY_SCOPE)
    .bind(command.idempotency_key().as_str())
    .bind(request_hash)
    .bind(response)
    .execute(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;
    if updated.rows_affected() != 1 {
        return Err(ReferralStoreError::Conflict);
    }
    Ok(())
}

#[derive(Serialize)]
struct RedemptionFingerprint<'a> {
    workspace_id: WorkspaceId,
    coupon_code: &'a str,
    order_reference: &'a str,
}

fn redemption_request_hash(command: &RedeemCouponCommand) -> Result<Vec<u8>, ReferralStoreError> {
    // Correlation and idempotency identifiers are intentionally excluded. A
    // legitimate HTTP retry receives a new request ID but must still replay the
    // original response when the business payload is unchanged.
    let fingerprint = RedemptionFingerprint {
        workspace_id: command.workspace_id(),
        coupon_code: command.coupon_code().as_str(),
        order_reference: command.order_reference(),
    };
    let encoded = serde_json::to_vec(&fingerprint).map_err(|_| ReferralStoreError::Unexpected)?;
    Ok(Sha256::digest(encoded).to_vec())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MerchRewardConfig {
    discount_percent: f64,
    expires_days: u32,
    #[serde(default)]
    code_prefix: Option<String>,
}

impl MerchRewardConfig {
    fn validate(&self) -> Result<(), ReferralStoreError> {
        if !(self.discount_percent.is_finite()
            && 0.0 < self.discount_percent
            && self.discount_percent <= 100.0
            && (1..=365).contains(&self.expires_days))
        {
            return Err(ReferralStoreError::Unexpected);
        }
        if self.code_prefix.as_ref().is_some_and(|prefix| {
            !(2..=16).contains(&prefix.len())
                || !prefix.bytes().all(|byte| byte.is_ascii_uppercase())
        }) {
            return Err(ReferralStoreError::Unexpected);
        }
        Ok(())
    }
}

/// Deterministic reward: a free physical item (for example an album CD/vinyl)
/// mailed to the fan once a referral threshold is reached. CrowdRelay never
/// collects or stores a shipping address; the `physical_reward.granted`
/// outbox event carries the fan's contact details so n8n can request an
/// address and hand the grant to fulfillment.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PhysicalRewardConfig {
    item_name: String,
    sku: String,
    expires_days: u32,
}

impl PhysicalRewardConfig {
    fn validate(&self) -> Result<(), ReferralStoreError> {
        let name_len = self.item_name.len();
        if self.item_name.trim() != self.item_name
            || !(1..=200).contains(&name_len)
            || self.item_name.chars().any(char::is_control)
        {
            return Err(ReferralStoreError::Unexpected);
        }
        if !(1..=64).contains(&self.sku.len())
            || !self
                .sku
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(ReferralStoreError::Unexpected);
        }
        if !(1..=365).contains(&self.expires_days) {
            return Err(ReferralStoreError::Unexpected);
        }
        Ok(())
    }
}

/// Deterministic reward configuration, parsed from `reward_rules.config`
/// according to `reward_rules.reward_type`.
enum RewardConfig {
    MerchDiscount(MerchRewardConfig),
    PhysicalItem(PhysicalRewardConfig),
}

impl RewardConfig {
    fn parse(reward_type: &str, config: serde_json::Value) -> Result<Self, ReferralStoreError> {
        match reward_type {
            "merch_discount" => Ok(Self::MerchDiscount(
                serde_json::from_value(config).map_err(|_| ReferralStoreError::Unexpected)?,
            )),
            "physical_item" => Ok(Self::PhysicalItem(
                serde_json::from_value(config).map_err(|_| ReferralStoreError::Unexpected)?,
            )),
            _ => Err(ReferralStoreError::Unexpected),
        }
    }

    fn validate(&self) -> Result<(), ReferralStoreError> {
        match self {
            Self::MerchDiscount(config) => config.validate(),
            Self::PhysicalItem(config) => config.validate(),
        }
    }

    const fn expires_days(&self) -> u32 {
        match self {
            Self::MerchDiscount(config) => config.expires_days,
            Self::PhysicalItem(config) => config.expires_days,
        }
    }
}

#[derive(Debug, FromRow)]
struct RewardRuleRow {
    id: Uuid,
    reward_type: String,
    threshold: Option<i32>,
    config: serde_json::Value,
    version: i32,
}

#[derive(Debug, FromRow)]
struct RewardOwnerRow {
    normalized_email: String,
    display_name: Option<String>,
}

#[derive(Debug, FromRow)]
struct IssuedCouponRow {
    id: Uuid,
    code_display: String,
}

#[derive(Debug, FromRow)]
struct DrawEntryRow {
    draw_id: Uuid,
    slug: String,
    name: String,
    prize_kind: String,
    closes_at: OffsetDateTime,
    draw_at: OffsetDateTime,
    qualified_referrals: i64,
    concert_checkins: i64,
    base_entries: i64,
    referral_entries: i64,
    checkin_entries: i64,
    total_entries: i64,
    max_entries: i64,
}

impl TryFrom<DrawEntryRow> for WeightedDrawEntry {
    type Error = ReferralStoreError;

    fn try_from(row: DrawEntryRow) -> Result<Self, Self::Error> {
        let prize_kind = match row.prize_kind.as_str() {
            "admission_pass" => RewardDrawPrizeKind::AdmissionPass,
            "physical_item" => RewardDrawPrizeKind::PhysicalItem,
            _ => return Err(ReferralStoreError::Unexpected),
        };
        Ok(Self {
            draw_id: RewardDrawId::from_uuid(row.draw_id),
            slug: row.slug,
            name: row.name,
            prize_kind,
            closes_at: row.closes_at,
            draw_at: row.draw_at,
            qualified_referrals: u64::try_from(row.qualified_referrals)
                .map_err(|_| ReferralStoreError::Unexpected)?,
            base_entries: u32::try_from(row.base_entries)
                .map_err(|_| ReferralStoreError::Unexpected)?,
            referral_entries: u32::try_from(row.referral_entries)
                .map_err(|_| ReferralStoreError::Unexpected)?,
            concert_checkins: u32::try_from(row.concert_checkins)
                .map_err(|_| ReferralStoreError::Unexpected)?,
            checkin_entries: u32::try_from(row.checkin_entries)
                .map_err(|_| ReferralStoreError::Unexpected)?,
            total_entries: u32::try_from(row.total_entries)
                .map_err(|_| ReferralStoreError::Unexpected)?,
            max_entries: u32::try_from(row.max_entries)
                .map_err(|_| ReferralStoreError::Unexpected)?,
        })
    }
}

#[derive(Debug, FromRow)]
struct CouponRow {
    id: Uuid,
    reward_grant_id: Uuid,
    reward_rule_id: Uuid,
    code_display: String,
    discount_percent: Option<f64>,
    max_uses: i32,
    used_count: i32,
    status: String,
    expires_at: Option<OffsetDateTime>,
}

impl TryFrom<CouponRow> for MerchCoupon {
    type Error = ReferralStoreError;

    fn try_from(row: CouponRow) -> Result<Self, Self::Error> {
        let status = parse_coupon_status(&row.status)?;
        let coupon = Self {
            id: MerchCouponId::from_uuid(row.id),
            reward_grant_id: RewardGrantId::from_uuid(row.reward_grant_id),
            reward_rule_id: RewardRuleId::from_uuid(row.reward_rule_id),
            code: CouponCode::parse(row.code_display)
                .map_err(|_| ReferralStoreError::Unexpected)?,
            discount_percent: row.discount_percent.ok_or(ReferralStoreError::Unexpected)?,
            max_uses: u32::try_from(row.max_uses).map_err(|_| ReferralStoreError::Unexpected)?,
            used_count: u32::try_from(row.used_count)
                .map_err(|_| ReferralStoreError::Unexpected)?,
            status,
            expires_at: row.expires_at,
        };
        coupon
            .validate()
            .map_err(|_| ReferralStoreError::Unexpected)?;
        Ok(coupon)
    }
}

#[derive(Debug, FromRow)]
struct PhysicalRewardRow {
    reward_grant_id: Uuid,
    reward_rule_id: Uuid,
    config: serde_json::Value,
    status: String,
    granted_at: OffsetDateTime,
    expires_at: Option<OffsetDateTime>,
}

impl TryFrom<PhysicalRewardRow> for PhysicalRewardGrant {
    type Error = ReferralStoreError;

    fn try_from(row: PhysicalRewardRow) -> Result<Self, Self::Error> {
        let config: PhysicalRewardConfig =
            serde_json::from_value(row.config).map_err(|_| ReferralStoreError::Unexpected)?;
        let status = match row.status.as_str() {
            "issued" => PhysicalRewardStatus::Issued,
            "delivered" => PhysicalRewardStatus::Fulfilled,
            "expired" => PhysicalRewardStatus::Expired,
            "revoked" => PhysicalRewardStatus::Revoked,
            _ => return Err(ReferralStoreError::Unexpected),
        };
        Ok(Self {
            reward_grant_id: RewardGrantId::from_uuid(row.reward_grant_id),
            reward_rule_id: RewardRuleId::from_uuid(row.reward_rule_id),
            item_name: config.item_name,
            sku: config.sku,
            status,
            granted_at: row.granted_at,
            expires_at: row.expires_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct RedeemableCouponRow {
    id: Uuid,
    reward_grant_id: Uuid,
    status: String,
    max_uses: i32,
    used_count: i32,
    expired: bool,
}

#[derive(Debug, FromRow)]
struct IdempotencyRow {
    request_hash: Vec<u8>,
    state: String,
    response_body: Option<String>,
    lease_expired: bool,
}

fn parse_coupon_status(value: &str) -> Result<CouponStatus, ReferralStoreError> {
    match value {
        "issued" => Ok(CouponStatus::Issued),
        "redeemed" => Ok(CouponStatus::Redeemed),
        "expired" => Ok(CouponStatus::Expired),
        "revoked" => Ok(CouponStatus::Revoked),
        _ => Err(ReferralStoreError::Unexpected),
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ReferralStoreError {
    Unavailable,
    NotFound,
    Conflict,
    Unexpected,
}

impl ReferralStoreError {
    fn from_sqlx(error: sqlx::Error) -> Self {
        match classify_sqlx_error(&error) {
            SqlxErrorClass::Unavailable => Self::Unavailable,
            SqlxErrorClass::NotFound => Self::NotFound,
            SqlxErrorClass::Conflict => Self::Conflict,
            SqlxErrorClass::Unexpected => Self::Unexpected,
        }
    }
}

impl From<ReferralStoreError> for RepositoryError {
    fn from(error: ReferralStoreError) -> Self {
        match error {
            ReferralStoreError::Unavailable => Self::Unavailable,
            ReferralStoreError::NotFound => Self::NotFound,
            ReferralStoreError::Conflict => Self::Conflict,
            ReferralStoreError::Unexpected => Self::Unexpected,
        }
    }
}

fn duration_as_milliseconds(duration: Duration) -> Result<i64, ReferralStoreError> {
    i64::try_from(duration.as_millis()).map_err(|_| ReferralStoreError::Unexpected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crowdrelay_application::{IdempotencyKey, RequestId};

    fn redemption_command(
        idempotency_key: &str,
        request_id: &str,
        order_reference: &str,
    ) -> Result<RedeemCouponCommand, Box<dyn std::error::Error>> {
        Ok(RedeemCouponCommand::new(
            WorkspaceId::from_uuid(Uuid::nil()),
            IdempotencyKey::parse(idempotency_key)?,
            RequestId::parse(request_id)?,
            CouponCode::parse("VIRYA-ABC12345")?,
            order_reference,
        )?)
    }

    #[test]
    fn redemption_fingerprint_ignores_transport_identifiers()
    -> Result<(), Box<dyn std::error::Error>> {
        let first = redemption_command("redeem-key-0001", "request-0001", "order-123")?;
        let retry = redemption_command("redeem-key-0001", "request-0002", "order-123")?;

        assert_eq!(
            redemption_request_hash(&first).map_err(|e| format!("hash error: {e:?}"))?,
            redemption_request_hash(&retry).map_err(|e| format!("hash error: {e:?}"))?
        );
        Ok(())
    }

    #[test]
    fn redemption_fingerprint_changes_with_business_payload()
    -> Result<(), Box<dyn std::error::Error>> {
        let first = redemption_command("redeem-key-0001", "request-0001", "order-123")?;
        let changed = redemption_command("redeem-key-0001", "request-0002", "order-124")?;

        assert_ne!(
            redemption_request_hash(&first).map_err(|e| format!("hash error: {e:?}"))?,
            redemption_request_hash(&changed).map_err(|e| format!("hash error: {e:?}"))?
        );
        Ok(())
    }

    #[test]
    fn reward_config_dispatches_on_reward_type() -> Result<(), Box<dyn std::error::Error>> {
        let merch = RewardConfig::parse(
            "merch_discount",
            serde_json::json!({"discount_percent": 15.0, "expires_days": 30, "code_prefix": "FAN"}),
        )
        .map_err(|e| format!("parse error: {e:?}"))?;
        assert!(merch.validate().is_ok());
        assert_eq!(merch.expires_days(), 30);
        assert!(matches!(merch, RewardConfig::MerchDiscount(_)));

        let physical = RewardConfig::parse(
            "physical_item",
            serde_json::json!({"item_name": "Virya — Signal (CD)", "sku": "virya-signal-cd", "expires_days": 60}),
        ).map_err(|e| format!("parse error: {e:?}"))?;
        assert!(physical.validate().is_ok());
        assert_eq!(physical.expires_days(), 60);
        assert!(matches!(physical, RewardConfig::PhysicalItem(_)));

        assert!(RewardConfig::parse("unknown_type", serde_json::json!({})).is_err());
        Ok(())
    }

    #[test]
    fn physical_reward_config_rejects_unsafe_or_out_of_range_values()
    -> Result<(), Box<dyn std::error::Error>> {
        let base = serde_json::json!({
            "item_name": "Virya — Signal (CD)",
            "sku": "virya-signal-cd",
            "expires_days": 30,
        });

        assert!(RewardConfig::parse("physical_item", base.clone()).is_ok());

        let mut blank_name = base.clone();
        blank_name["item_name"] = serde_json::json!("");
        assert!(
            RewardConfig::parse("physical_item", blank_name)
                .map_err(|e| format!("parse error: {e:?}"))?
                .validate()
                .is_err()
        );

        let mut bad_sku = base.clone();
        bad_sku["sku"] = serde_json::json!("has a space");
        assert!(
            RewardConfig::parse("physical_item", bad_sku)
                .map_err(|e| format!("parse error: {e:?}"))?
                .validate()
                .is_err()
        );

        let mut zero_days = base;
        zero_days["expires_days"] = serde_json::json!(0);
        assert!(
            RewardConfig::parse("physical_item", zero_days)
                .map_err(|e| format!("parse error: {e:?}"))?
                .validate()
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn physical_reward_row_maps_lifecycle_status_and_config()
    -> Result<(), Box<dyn std::error::Error>> {
        let row = PhysicalRewardRow {
            reward_grant_id: Uuid::nil(),
            reward_rule_id: Uuid::nil(),
            config: serde_json::json!({
                "item_name": "Virya — Signal (CD)",
                "sku": "virya-signal-cd",
                "expires_days": 60,
            }),
            status: "delivered".to_owned(),
            granted_at: OffsetDateTime::UNIX_EPOCH,
            expires_at: None,
        };

        let grant =
            PhysicalRewardGrant::try_from(row).map_err(|e| format!("try_from error: {e:?}"))?;
        assert_eq!(grant.item_name, "Virya — Signal (CD)");
        assert_eq!(grant.sku, "virya-signal-cd");
        assert_eq!(grant.status, PhysicalRewardStatus::Fulfilled);
        Ok(())
    }
}
