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

impl PostgresReferralRepository {
    #[must_use]
    pub fn new(pool: PgPool, workspace_slug: WorkspaceSlug, database: &DatabaseConfig) -> Self {
        Self {
            pool,
            workspace_slug,
            operation_timeout: database.operation_timeout,
            lock_timeout: database.lock_timeout,
        }
    }

    async fn bounded<T>(
        &self,
        operation: impl Future<Output = Result<T, ReferralStoreError>>,
    ) -> Result<T, ReferralStoreError> {
        timeout(self.operation_timeout, operation)
            .await
            .map_err(|_| ReferralStoreError::Unavailable)?
    }

    async fn trusted_workspace_id(&self) -> Result<WorkspaceId, ReferralStoreError> {
        let id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM workspaces WHERE slug = $1")
            .bind(self.workspace_slug.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(ReferralStoreError::from_sqlx)?
            .ok_or(ReferralStoreError::NotFound)?;
        Ok(WorkspaceId::from_uuid(id))
    }

    async fn referral_code_is_active_inner(
        &self,
        workspace_id: WorkspaceId,
        code: &ReferralCode,
    ) -> Result<bool, ReferralStoreError> {
        if self.trusted_workspace_id().await? != workspace_id {
            return Err(ReferralStoreError::NotFound);
        }
        sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM referral_codes
                INNER JOIN fans
                    ON fans.workspace_id = referral_codes.workspace_id
                    AND fans.id = referral_codes.fan_id
                WHERE referral_codes.workspace_id = $1
                    AND referral_codes.code = $2
                    AND referral_codes.active
                    AND fans.status = 'active'
            )
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(code.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(ReferralStoreError::from_sqlx)
    }

    async fn load_referral_progress_inner(
        &self,
        workspace_id: WorkspaceId,
        session_token: &FanSessionToken,
    ) -> Result<ReferralProgress, ReferralStoreError> {
        if self.trusted_workspace_id().await? != workspace_id {
            return Err(ReferralStoreError::NotFound);
        }
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(ReferralStoreError::from_sqlx)?;
        configure_transaction(&mut transaction, self.operation_timeout, self.lock_timeout).await?;

        let fan_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            UPDATE fan_sessions
            SET last_seen_at = now()
            WHERE workspace_id = $1
                AND session_token_hash = digest($2, 'sha256')
                AND revoked_at IS NULL
                AND expires_at > now()
            RETURNING fan_id
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(session_token.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(ReferralStoreError::from_sqlx)?
        .ok_or(ReferralStoreError::NotFound)?;

        let code = sqlx::query_scalar::<_, String>(
            r#"
            SELECT code
            FROM referral_codes
            WHERE workspace_id = $1 AND fan_id = $2 AND active
            ORDER BY created_at, id
            LIMIT 1
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(ReferralStoreError::from_sqlx)?
        .ok_or(ReferralStoreError::NotFound)?;

        let (qualified, pending) = sqlx::query_as::<_, (i64, i64)>(
            r#"
            SELECT
                count(*) FILTER (WHERE status = 'qualified')::bigint,
                count(*) FILTER (WHERE status = 'pending')::bigint
            FROM referral_attributions
            WHERE workspace_id = $1 AND referrer_fan_id = $2
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(ReferralStoreError::from_sqlx)?;

        let next_threshold = sqlx::query_scalar::<_, Option<i32>>(
            r#"
            SELECT min(threshold)
            FROM reward_rules
            WHERE workspace_id = $1
                AND active
                AND reward_type IN ('merch_discount', 'physical_item')
                AND threshold::bigint > $2
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(qualified)
        .fetch_one(&mut *transaction)
        .await
        .map_err(ReferralStoreError::from_sqlx)?;

        let draw_entry_rows = sqlx::query_as::<_, DrawEntryRow>(
            r#"
            SELECT
                draw.id AS draw_id,
                draw.slug,
                draw.name,
                draw.prize_kind,
                draw.closes_at,
                draw.draw_at,
                referral_count.qualified_referrals,
                checkin_count.concert_checkins,
                draw.base_entries::bigint AS base_entries,
                LEAST(
                    (draw.max_entries - draw.base_entries)::bigint,
                    referral_count.qualified_referrals * draw.entries_per_referral::bigint
                ) AS referral_entries,
                LEAST(
                    GREATEST(
                        (draw.max_entries - draw.base_entries)::bigint
                            - LEAST(
                                (draw.max_entries - draw.base_entries)::bigint,
                                referral_count.qualified_referrals * draw.entries_per_referral::bigint
                            ),
                        0
                    ),
                    checkin_count.concert_checkins * draw.entries_per_checkin::bigint
                ) AS checkin_entries,
                draw.base_entries::bigint
                    + LEAST(
                        (draw.max_entries - draw.base_entries)::bigint,
                        referral_count.qualified_referrals * draw.entries_per_referral::bigint
                    )
                    + LEAST(
                        GREATEST(
                            (draw.max_entries - draw.base_entries)::bigint
                                - LEAST(
                                    (draw.max_entries - draw.base_entries)::bigint,
                                    referral_count.qualified_referrals * draw.entries_per_referral::bigint
                                ),
                            0
                        ),
                        checkin_count.concert_checkins * draw.entries_per_checkin::bigint
                    ) AS total_entries,
                draw.max_entries::bigint AS max_entries
            FROM reward_draws AS draw
            CROSS JOIN LATERAL (
                SELECT count(*)::bigint AS qualified_referrals
                FROM referral_attributions AS attribution
                WHERE attribution.workspace_id = draw.workspace_id
                  AND attribution.referrer_fan_id = $2
                  AND attribution.status = 'qualified'
                  AND attribution.qualified_at <= draw.closes_at
            ) AS referral_count
            CROSS JOIN LATERAL (
                SELECT count(*)::bigint AS concert_checkins
                FROM concert_checkins AS checkin
                WHERE checkin.workspace_id = draw.workspace_id
                  AND checkin.fan_id = $2
                  AND checkin.checked_in_at >= draw.opens_at
                  AND checkin.checked_in_at <= draw.closes_at
            ) AS checkin_count
            WHERE draw.workspace_id = $1
              AND draw.status IN ('scheduled', 'running')
              AND draw.opens_at <= now()
              AND draw.closes_at > now()
              AND (
                  draw.eligibility_kind = 'all_active'
                  OR EXISTS (
                      SELECT 1
                      FROM event_interests AS interest
                      WHERE interest.workspace_id = draw.workspace_id
                        AND interest.event_id = draw.event_id
                        AND interest.fan_id = $2
                  )
              )
            ORDER BY draw.closes_at, draw.id
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id)
        .fetch_all(&mut *transaction)
        .await
        .map_err(ReferralStoreError::from_sqlx)?;

        let rows = sqlx::query_as::<_, CouponRow>(
            r#"
            SELECT
                merch_coupons.id,
                merch_coupons.reward_grant_id,
                reward_grants.reward_rule_id,
                merch_coupons.code_display,
                merch_coupons.discount_percent::double precision AS discount_percent,
                merch_coupons.max_uses,
                merch_coupons.used_count,
                CASE
                    WHEN merch_coupons.status = 'issued'
                        AND merch_coupons.expires_at <= now()
                    THEN 'expired'
                    ELSE merch_coupons.status
                END AS status,
                merch_coupons.expires_at
            FROM merch_coupons
            INNER JOIN reward_grants
                ON reward_grants.workspace_id = merch_coupons.workspace_id
                AND reward_grants.id = merch_coupons.reward_grant_id
            WHERE merch_coupons.workspace_id = $1
                AND reward_grants.fan_id = $2
            ORDER BY merch_coupons.created_at DESC, merch_coupons.id DESC
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id)
        .fetch_all(&mut *transaction)
        .await
        .map_err(ReferralStoreError::from_sqlx)?;

        let physical_reward_rows = sqlx::query_as::<_, PhysicalRewardRow>(
            r#"
            SELECT
                reward_grants.id AS reward_grant_id,
                reward_grants.reward_rule_id,
                reward_rules.config,
                CASE
                    WHEN reward_grants.status = 'issued'
                        AND reward_grants.expires_at <= now()
                    THEN 'expired'
                    ELSE reward_grants.status
                END AS status,
                reward_grants.issued_at AS granted_at,
                reward_grants.expires_at
            FROM reward_grants
            INNER JOIN reward_rules
                ON reward_rules.workspace_id = reward_grants.workspace_id
                AND reward_rules.id = reward_grants.reward_rule_id
            WHERE reward_grants.workspace_id = $1
                AND reward_grants.fan_id = $2
                AND reward_rules.reward_type = 'physical_item'
            ORDER BY reward_grants.created_at DESC, reward_grants.id DESC
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id)
        .fetch_all(&mut *transaction)
        .await
        .map_err(ReferralStoreError::from_sqlx)?;

        transaction
            .commit()
            .await
            .map_err(ReferralStoreError::from_sqlx)?;

        let draw_entries = draw_entry_rows
            .into_iter()
            .map(WeightedDrawEntry::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let coupons = rows
            .into_iter()
            .map(MerchCoupon::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let physical_rewards = physical_reward_rows
            .into_iter()
            .map(PhysicalRewardGrant::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ReferralProgress {
            referral_code: ReferralCode::parse(code).map_err(|_| ReferralStoreError::Unexpected)?,
            qualified_referrals: u64::try_from(qualified)
                .map_err(|_| ReferralStoreError::Unexpected)?,
            pending_referrals: u64::try_from(pending)
                .map_err(|_| ReferralStoreError::Unexpected)?,
            next_reward_threshold: next_threshold
                .map(u32::try_from)
                .transpose()
                .map_err(|_| ReferralStoreError::Unexpected)?,
            draw_entries,
            coupons,
            physical_rewards,
        })
    }

    async fn redeem_coupon_inner(
        &self,
        command: &RedeemCouponCommand,
    ) -> Result<CouponRedemptionResult, ReferralStoreError> {
        let request_hash = redemption_request_hash(command)?;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(ReferralStoreError::from_sqlx)?;
        configure_transaction(&mut transaction, self.operation_timeout, self.lock_timeout).await?;

        let workspace_id =
            trusted_workspace_id_in_transaction(&mut transaction, &self.workspace_slug).await?;
        if workspace_id != command.workspace_id() {
            return Err(ReferralStoreError::NotFound);
        }

        let inserted = start_idempotency(
            &mut transaction,
            workspace_id,
            command,
            &request_hash,
            self.operation_timeout,
        )
        .await?;
        let idempotency = lock_idempotency(&mut transaction, workspace_id, command).await?;
        if idempotency.request_hash != request_hash {
            return Err(ReferralStoreError::Conflict);
        }
        if idempotency.state == "completed" {
            let body = idempotency
                .response_body
                .ok_or(ReferralStoreError::Unexpected)?;
            let result = serde_json::from_str(&body).map_err(|_| ReferralStoreError::Unexpected)?;
            transaction
                .commit()
                .await
                .map_err(ReferralStoreError::from_sqlx)?;
            return Ok(result);
        }
        if !inserted && !idempotency.lease_expired {
            return Err(ReferralStoreError::Conflict);
        }
        if !inserted {
            reclaim_idempotency(
                &mut transaction,
                workspace_id,
                command,
                self.operation_timeout,
            )
            .await?;
        }

        let row = sqlx::query_as::<_, RedeemableCouponRow>(
            r#"
            SELECT
                merch_coupons.id,
                merch_coupons.reward_grant_id,
                merch_coupons.status,
                merch_coupons.max_uses,
                merch_coupons.used_count,
                coalesce(merch_coupons.expires_at <= now(), false) AS expired
            FROM merch_coupons
            WHERE merch_coupons.workspace_id = $1
                AND merch_coupons.code_hash = digest($2, 'sha256')
            FOR UPDATE
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(command.coupon_code().as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(ReferralStoreError::from_sqlx)?
        .ok_or(ReferralStoreError::NotFound)?;

        if row.status != "issued" || row.used_count >= row.max_uses {
            return Err(ReferralStoreError::Conflict);
        }
        if row.expired {
            // Runtime validation is authoritative even before a later
            // maintenance job materializes the expired status. Do not mutate
            // it in a transaction that is intentionally rolled back here.
            return Err(ReferralStoreError::Conflict);
        }

        let used_count = row
            .used_count
            .checked_add(1)
            .ok_or(ReferralStoreError::Unexpected)?;
        let status = if used_count == row.max_uses {
            CouponStatus::Redeemed
        } else {
            CouponStatus::Issued
        };
        let redeemed_at = OffsetDateTime::now_utc();

        sqlx::query(
            r#"
            INSERT INTO coupon_redemptions (
                workspace_id,
                coupon_id,
                order_reference,
                usage_number,
                redeemed_at,
                request_id
            )
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(row.id)
        .bind(command.order_reference())
        .bind(used_count)
        .bind(redeemed_at)
        .bind(command.request_id().as_str())
        .execute(&mut *transaction)
        .await
        .map_err(ReferralStoreError::from_sqlx)?;

        sqlx::query(
            r#"
            UPDATE merch_coupons
            SET
                used_count = $3,
                status = CASE WHEN $3 = max_uses THEN 'redeemed' ELSE 'issued' END,
                redeemed_at = CASE WHEN $3 = max_uses THEN $4 ELSE redeemed_at END,
                last_order_reference = $5
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(row.id)
        .bind(used_count)
        .bind(redeemed_at)
        .bind(command.order_reference())
        .execute(&mut *transaction)
        .await
        .map_err(ReferralStoreError::from_sqlx)?;

        if status == CouponStatus::Redeemed {
            sqlx::query(
                r#"
                UPDATE reward_grants
                SET status = 'redeemed', redeemed_at = $3
                WHERE workspace_id = $1 AND id = $2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(row.reward_grant_id)
            .bind(redeemed_at)
            .execute(&mut *transaction)
            .await
            .map_err(ReferralStoreError::from_sqlx)?;
        }

        append_outbox(
            &mut transaction,
            workspace_id,
            "merch_coupon.redeemed",
            command.request_id().as_str(),
            json!({
                "workspace_id": workspace_id,
                "coupon_id": row.id,
                "reward_grant_id": row.reward_grant_id,
                "order_reference": command.order_reference(),
                "used_count": used_count,
                "max_uses": row.max_uses,
                "redeemed_at": redeemed_at,
            }),
        )
        .await?;

        let result = CouponRedemptionResult {
            coupon_id: MerchCouponId::from_uuid(row.id),
            reward_grant_id: RewardGrantId::from_uuid(row.reward_grant_id),
            status,
            used_count: u32::try_from(used_count).map_err(|_| ReferralStoreError::Unexpected)?,
            max_uses: u32::try_from(row.max_uses).map_err(|_| ReferralStoreError::Unexpected)?,
            redeemed_at,
        };
        complete_idempotency(
            &mut transaction,
            workspace_id,
            command,
            &request_hash,
            &result,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(ReferralStoreError::from_sqlx)?;
        Ok(result)
    }
}

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

/// Creates a fresh privacy-safe fan session and returns the opaque token once.
pub(crate) async fn issue_fan_session(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    fan_id: FanId,
) -> Result<FanSessionToken, ReferralStoreError> {
    let token = sqlx::query_scalar::<_, String>(
        r#"
        WITH token AS (
            SELECT encode(gen_random_bytes(32), 'hex') AS value
        ), inserted AS (
            INSERT INTO fan_sessions (
                workspace_id,
                fan_id,
                session_token_hash,
                expires_at
            )
            SELECT $1, $2, digest(token.value, 'sha256'),
                now() + ($3::bigint * interval '1 day')
            FROM token
            RETURNING session_token_hash
        )
        SELECT token.value
        FROM token, inserted
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id.into_uuid())
    .bind(FAN_SESSION_TTL_DAYS)
    .fetch_one(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;
    FanSessionToken::parse(token).map_err(|_| ReferralStoreError::Unexpected)
}

/// Records an attribution that will count only after inbox confirmation.
pub(crate) async fn record_pending_signup_referral(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    referred_fan_id: FanId,
    referral_code_id: Option<Uuid>,
    referrer_fan_id: Option<Uuid>,
) -> Result<(), ReferralStoreError> {
    let (Some(referral_code_id), Some(referrer_fan_id)) = (referral_code_id, referrer_fan_id)
    else {
        return Ok(());
    };
    if referrer_fan_id == referred_fan_id.into_uuid() {
        return Ok(());
    }

    sqlx::query(
        r#"
        INSERT INTO referral_attributions (
            workspace_id,
            referrer_fan_id,
            referred_fan_id,
            referral_code_id,
            accepted_at,
            status,
            qualification_reason
        )
        SELECT
            $1, $2, $3, $4, now(), 'pending', 'awaiting_confirmation'
        WHERE EXISTS (
            SELECT 1 FROM fans
            WHERE workspace_id = $1 AND id = $2 AND status = 'active'
        )
        AND EXISTS (
            SELECT 1 FROM fans
            WHERE workspace_id = $1 AND id = $3 AND status = 'pending'
        )
        ON CONFLICT (workspace_id, referred_fan_id) DO NOTHING
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(referrer_fan_id)
    .bind(referred_fan_id.into_uuid())
    .bind(referral_code_id)
    .execute(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;
    Ok(())
}

/// Promotes valid signup attribution into one qualified referral and evaluates
/// every deterministic reward rule whose threshold has been reached.
pub(crate) async fn qualify_signup_referral_and_rewards(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    referred_fan_id: FanId,
    referral_code_id: Option<Uuid>,
    referrer_fan_id: Option<Uuid>,
    request_id: &str,
) -> Result<(), ReferralStoreError> {
    let (Some(referral_code_id), Some(referrer_fan_id)) = (referral_code_id, referrer_fan_id)
    else {
        return Ok(());
    };
    if referrer_fan_id == referred_fan_id.into_uuid() {
        return Ok(());
    }

    // Serialize qualification and threshold evaluation per referrer. Without
    // this lock, two concurrent signups could both observe a count below the
    // threshold and commit without granting the reward. The next statement
    // receives a fresh READ COMMITTED snapshot after any previous holder exits.
    sqlx::query(
        r#"
        SELECT pg_advisory_xact_lock(
            hashtextextended($1::uuid::text || ':' || $2::uuid::text, 0)
        )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(referrer_fan_id)
    .execute(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;

    let qualified = sqlx::query_as::<_, (Uuid, OffsetDateTime)>(
        r#"
        INSERT INTO referral_attributions (
            workspace_id,
            referrer_fan_id,
            referred_fan_id,
            referral_code_id,
            accepted_at,
            status,
            qualification_reason,
            qualified_at
        )
        SELECT
            $1, $2, $3, $4, now(), 'qualified',
            'active_fan_signup', now()
        WHERE EXISTS (
            SELECT 1 FROM fans
            WHERE workspace_id = $1 AND id = $2 AND status = 'active'
        )
        AND EXISTS (
            SELECT 1 FROM fans
            WHERE workspace_id = $1 AND id = $3 AND status = 'active'
        )
        ON CONFLICT (workspace_id, referred_fan_id) DO UPDATE
        SET
            status = 'qualified',
            qualification_reason = 'confirmed_fan_signup',
            qualified_at = now()
        WHERE referral_attributions.status = 'pending'
            AND referral_attributions.referrer_fan_id = EXCLUDED.referrer_fan_id
            AND referral_attributions.referral_code_id = EXCLUDED.referral_code_id
        RETURNING id, qualified_at
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(referrer_fan_id)
    .bind(referred_fan_id.into_uuid())
    .bind(referral_code_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;

    let Some((attribution_id, qualified_at)) = qualified else {
        return Ok(());
    };

    append_outbox(
        transaction,
        workspace_id,
        "referral.qualified",
        request_id,
        json!({
            "workspace_id": workspace_id,
            "attribution_id": attribution_id,
            "referrer_fan_id": referrer_fan_id,
            "referred_fan_id": referred_fan_id,
            "qualified_at": qualified_at,
        }),
    )
    .await?;

    let qualified_count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)::bigint
        FROM referral_attributions
        WHERE workspace_id = $1
            AND referrer_fan_id = $2
            AND status = 'qualified'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(referrer_fan_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;

    let rules = sqlx::query_as::<_, RewardRuleRow>(
        r#"
        SELECT id, reward_type, threshold, config, version
        FROM reward_rules
        WHERE workspace_id = $1
            AND active
            AND reward_type IN ('merch_discount', 'physical_item')
            AND threshold IS NOT NULL
            AND threshold::bigint <= $2
        ORDER BY threshold, id
        FOR SHARE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(qualified_count)
    .fetch_all(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;

    if rules.is_empty() {
        return Ok(());
    }

    let owner = sqlx::query_as::<_, RewardOwnerRow>(
        r#"
        SELECT normalized_email, display_name
        FROM fans
        WHERE workspace_id = $1 AND id = $2 AND status = 'active'
        FOR SHARE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(referrer_fan_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?
    .ok_or(ReferralStoreError::NotFound)?;

    for rule in rules {
        let threshold = rule.threshold.ok_or(ReferralStoreError::Unexpected)?;
        let config = RewardConfig::parse(&rule.reward_type, rule.config)?;
        config.validate()?;
        let qualification_key = format!("qualified-referrals:{threshold}:v{}", rule.version);
        let expires_at = OffsetDateTime::now_utc()
            .checked_add(time::Duration::days(i64::from(config.expires_days())))
            .ok_or(ReferralStoreError::Unexpected)?;

        let grant_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO reward_grants (
                workspace_id,
                fan_id,
                reward_rule_id,
                qualification_key,
                status,
                issued_at,
                expires_at
            )
            VALUES ($1, $2, $3, $4, 'issued', now(), $5)
            ON CONFLICT (
                workspace_id,
                reward_rule_id,
                fan_id,
                qualification_key
            ) DO UPDATE
            SET
                status = 'issued',
                issued_at = now(),
                expires_at = EXCLUDED.expires_at,
                revoked_at = NULL
            WHERE reward_grants.status = 'revoked'
            RETURNING id
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(referrer_fan_id)
        .bind(rule.id)
        .bind(&qualification_key)
        .bind(expires_at)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(ReferralStoreError::from_sqlx)?;

        let Some(grant_id) = grant_id else {
            continue;
        };

        append_outbox(
            transaction,
            workspace_id,
            "reward.granted",
            request_id,
            json!({
                "workspace_id": workspace_id,
                "reward_grant_id": grant_id,
                "reward_rule_id": rule.id,
                "fan_id": referrer_fan_id,
                "qualified_referral_count": qualified_count,
                "threshold": threshold,
                "expires_at": expires_at,
            }),
        )
        .await?;

        match config {
            RewardConfig::MerchDiscount(config) => {
                let prefix = config.code_prefix.as_deref().unwrap_or("FAN");
                let coupon = sqlx::query_as::<_, IssuedCouponRow>(
                    r#"
                    WITH material AS (
                        SELECT $4 || '-' || upper(encode(gen_random_bytes(10), 'hex')) AS code
                    )
                    INSERT INTO merch_coupons (
                        workspace_id,
                        reward_grant_id,
                        code_hash,
                        code_display,
                        discount_percent,
                        max_uses,
                        expires_at,
                        status
                    )
                    SELECT
                        $1, $2, digest(material.code, 'sha256'), material.code,
                        ($3::double precision)::numeric(5,2), 1, $5, 'issued'
                    FROM material
                    ON CONFLICT (workspace_id, reward_grant_id) DO UPDATE
                    SET
                        code_hash = EXCLUDED.code_hash,
                        code_display = EXCLUDED.code_display,
                        discount_percent = EXCLUDED.discount_percent,
                        max_uses = EXCLUDED.max_uses,
                        used_count = 0,
                        expires_at = EXCLUDED.expires_at,
                        status = 'issued',
                        redeemed_at = NULL,
                        revoked_at = NULL,
                        last_order_reference = NULL
                    WHERE merch_coupons.status = 'revoked'
                    RETURNING id, code_display
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(grant_id)
                .bind(config.discount_percent)
                .bind(prefix)
                .bind(expires_at)
                .fetch_one(&mut **transaction)
                .await
                .map_err(ReferralStoreError::from_sqlx)?;

                append_outbox(
                    transaction,
                    workspace_id,
                    "merch_coupon.issued",
                    request_id,
                    json!({
                        "workspace_id": workspace_id,
                        "coupon_id": coupon.id,
                        "reward_grant_id": grant_id,
                        "fan_id": referrer_fan_id,
                        "email": &owner.normalized_email,
                        "display_name": &owner.display_name,
                        "coupon_code": &coupon.code_display,
                        "discount_percent": config.discount_percent,
                        "max_uses": 1,
                        "expires_at": expires_at,
                        "qualified_referral_count": qualified_count,
                    }),
                )
                .await?;
            }
            RewardConfig::PhysicalItem(config) => {
                // Physical fulfillment (collecting a shipping address, packing
                // and shipping the item) happens outside CrowdRelay. n8n owns
                // fan-facing mail and export workflows; see docs/ARCHITECTURE.md.
                append_outbox(
                    transaction,
                    workspace_id,
                    "physical_reward.granted",
                    request_id,
                    json!({
                        "workspace_id": workspace_id,
                        "reward_grant_id": grant_id,
                        "reward_rule_id": rule.id,
                        "fan_id": referrer_fan_id,
                        "email": &owner.normalized_email,
                        "display_name": &owner.display_name,
                        "item_name": config.item_name,
                        "sku": config.sku,
                        "expires_at": expires_at,
                        "qualified_referral_count": qualified_count,
                    }),
                )
                .await?;
            }
        }
    }

    Ok(())
}

/// Reverses a qualified referral when its referred fan withdraws consent.
///
/// Already redeemed coupons and fulfilled rewards remain immutable accounting
/// records. Only still-issued grants above the new qualified count are revoked.
pub(crate) async fn reverse_signup_referral_and_rewards(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    referred_fan_id: FanId,
    request_id: &str,
) -> Result<(), ReferralStoreError> {
    let attribution = sqlx::query_as::<_, (Uuid, Uuid)>(
        r#"
        SELECT id, referrer_fan_id
        FROM referral_attributions
        WHERE workspace_id = $1
            AND referred_fan_id = $2
            AND status = 'qualified'
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(referred_fan_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;
    let Some((attribution_id, referrer_fan_id)) = attribution else {
        return Ok(());
    };

    sqlx::query(
        r#"
        SELECT pg_advisory_xact_lock(
            hashtextextended($1::uuid::text || ':' || $2::uuid::text, 0)
        )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(referrer_fan_id)
    .execute(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;

    let changed = sqlx::query(
        r#"
        UPDATE referral_attributions
        SET
            status = 'reversed',
            qualification_reason = 'fan_unsubscribed',
            reversed_at = now()
        WHERE workspace_id = $1
            AND id = $2
            AND status = 'qualified'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(attribution_id)
    .execute(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;
    if changed.rows_affected() != 1 {
        return Ok(());
    }

    let qualified_count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)::bigint
        FROM referral_attributions
        WHERE workspace_id = $1
            AND referrer_fan_id = $2
            AND status = 'qualified'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(referrer_fan_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;

    let revoked_coupons = sqlx::query(
        r#"
        UPDATE merch_coupons AS coupon
        SET
            status = 'revoked',
            revoked_at = now()
        FROM reward_grants AS reward_grant
        INNER JOIN reward_rules AS rule
            ON rule.workspace_id = reward_grant.workspace_id
            AND rule.id = reward_grant.reward_rule_id
        WHERE coupon.workspace_id = $1
            AND coupon.workspace_id = reward_grant.workspace_id
            AND coupon.reward_grant_id = reward_grant.id
            AND reward_grant.fan_id = $2
            AND rule.threshold::bigint > $3
            AND coupon.status = 'issued'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(referrer_fan_id)
    .bind(qualified_count)
    .execute(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?
    .rows_affected();

    let revoked_grants = sqlx::query(
        r#"
        UPDATE reward_grants AS reward_grant
        SET
            status = 'revoked',
            revoked_at = now()
        FROM reward_rules AS rule
        WHERE reward_grant.workspace_id = $1
            AND reward_grant.fan_id = $2
            AND reward_grant.status = 'issued'
            AND rule.workspace_id = reward_grant.workspace_id
            AND rule.id = reward_grant.reward_rule_id
            AND rule.threshold::bigint > $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(referrer_fan_id)
    .bind(qualified_count)
    .execute(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?
    .rows_affected();

    append_outbox(
        transaction,
        workspace_id,
        "referral.reversed",
        request_id,
        json!({
            "workspace_id": workspace_id,
            "attribution_id": attribution_id,
            "referrer_fan_id": referrer_fan_id,
            "referred_fan_id": referred_fan_id,
            "qualified_referral_count": qualified_count,
            "revoked_grant_count": revoked_grants,
            "revoked_coupon_count": revoked_coupons,
        }),
    )
    .await
}

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
