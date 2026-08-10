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
