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

async fn canonical_fan_pair(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    referrer_fan_id: Uuid,
    referred_fan_id: Uuid,
) -> Result<Option<(Uuid, Uuid)>, ReferralStoreError> {
    let (referrer, referred) = sqlx::query_as::<_, (Option<Uuid>, Option<Uuid>)>(
        "SELECT canonical_fan_id($1,$2), canonical_fan_id($1,$3)",
    )
    .bind(workspace_id.into_uuid())
    .bind(referrer_fan_id)
    .bind(referred_fan_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;
    let (Some(referrer), Some(referred)) = (referrer, referred) else {
        return Ok(None);
    };
    if referrer == referred {
        return Ok(None);
    }
    Ok(Some((referrer, referred)))
}

/// Reconcile referral rewards for one canonical referrer.
///
/// This is deliberately idempotent and side-effect safe inside the caller's
/// transaction: already-issued grants produce no new outbox event, redeemed or
/// delivered accounting is immutable, and only still-issued excess/duplicate
/// grants are revoked.
pub(crate) async fn sync_referral_rewards_for_referrer(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    referrer_fan_id: Uuid,
    request_id: &str,
) -> Result<(i64, u64, u64), ReferralStoreError> {
    let canonical_referrer_fan_id = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT canonical_fan_id($1,$2)",
    )
    .bind(workspace_id.into_uuid())
    .bind(referrer_fan_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?
    .ok_or(ReferralStoreError::NotFound)?;

    sqlx::query(
        "SELECT pg_advisory_xact_lock(
             hashtextextended($1::uuid::text || ':' || $2::uuid::text, 0)
         )",
    )
    .bind(workspace_id.into_uuid())
    .bind(canonical_referrer_fan_id)
    .execute(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;

    let qualified_count = sqlx::query_scalar::<_, i64>(
        "SELECT canonical_qualified_referral_count($1,$2,NULL)",
    )
    .bind(workspace_id.into_uuid())
    .bind(canonical_referrer_fan_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;

    // Revoke coupon material first while its issued grant still names the
    // exact entitlement. A non-canonical issued grant can only remain after
    // merge when the survivor already had the same unique reward key.
    let revoked_coupons = sqlx::query(
        r#"
        WITH doomed AS (
            SELECT grant_row.id
            FROM reward_grants grant_row
            JOIN reward_rules rule
              ON rule.workspace_id = grant_row.workspace_id
             AND rule.id = grant_row.reward_rule_id
            WHERE grant_row.workspace_id = $1
              AND grant_row.fan_id IN (
                  SELECT fan_id FROM canonical_fan_family($1,$2)
              )
              AND grant_row.status = 'issued'
              AND (
                  rule.threshold::bigint > $3
                  OR grant_row.fan_id <> $2
                  OR EXISTS (
                      SELECT 1
                      FROM reward_grants fulfilled
                      WHERE fulfilled.workspace_id = grant_row.workspace_id
                        AND fulfilled.fan_id IN (
                            SELECT fan_id FROM canonical_fan_family($1,$2)
                        )
                        AND fulfilled.reward_rule_id = grant_row.reward_rule_id
                        AND fulfilled.qualification_key = grant_row.qualification_key
                        AND fulfilled.status IN ('delivered','redeemed')
                  )
              )
        )
        UPDATE merch_coupons coupon
        SET status = 'revoked', revoked_at = now()
        FROM doomed
        WHERE coupon.workspace_id = $1
          AND coupon.reward_grant_id = doomed.id
          AND coupon.status = 'issued'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(canonical_referrer_fan_id)
    .bind(qualified_count)
    .execute(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?
    .rows_affected();

    let revoked_grants = sqlx::query(
        r#"
        UPDATE reward_grants grant_row
        SET status = 'revoked', revoked_at = now()
        FROM reward_rules rule
        WHERE grant_row.workspace_id = $1
          AND grant_row.fan_id IN (
              SELECT fan_id FROM canonical_fan_family($1,$2)
          )
          AND grant_row.status = 'issued'
          AND rule.workspace_id = grant_row.workspace_id
          AND rule.id = grant_row.reward_rule_id
          AND (
              rule.threshold::bigint > $3
              OR grant_row.fan_id <> $2
              OR EXISTS (
                  SELECT 1
                  FROM reward_grants fulfilled
                  WHERE fulfilled.workspace_id = grant_row.workspace_id
                    AND fulfilled.fan_id IN (
                        SELECT fan_id FROM canonical_fan_family($1,$2)
                    )
                    AND fulfilled.reward_rule_id = grant_row.reward_rule_id
                    AND fulfilled.qualification_key = grant_row.qualification_key
                    AND fulfilled.status IN ('delivered','redeemed')
              )
          )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(canonical_referrer_fan_id)
    .bind(qualified_count)
    .execute(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?
    .rows_affected();

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
        return Ok((qualified_count, revoked_grants, revoked_coupons));
    }

    let owner = sqlx::query_as::<_, RewardOwnerRow>(
        r#"
        SELECT normalized_email, display_name
        FROM fans
        WHERE workspace_id = $1
          AND id = $2
          AND status = 'active'
          AND deleted_at IS NULL
          AND merged_into_fan_id IS NULL
        FOR SHARE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(canonical_referrer_fan_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;
    let Some(owner) = owner else {
        return Ok((qualified_count, revoked_grants, revoked_coupons));
    };

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
                workspace_id, fan_id, reward_rule_id, qualification_key,
                status, issued_at, expires_at
            )
            SELECT $1, $2, $3, $4, 'issued', now(), $5
            WHERE NOT EXISTS (
                SELECT 1
                FROM reward_grants prior
                WHERE prior.workspace_id = $1
                  AND prior.fan_id IN (
                      SELECT fan_id FROM canonical_fan_family($1,$2)
                  )
                  AND prior.reward_rule_id = $3
                  AND prior.qualification_key = $4
                  AND prior.status IN ('delivered','redeemed')
            )
            ON CONFLICT (
                workspace_id, reward_rule_id, fan_id, qualification_key
            ) DO UPDATE
            SET status = 'issued',
                issued_at = now(),
                expires_at = EXCLUDED.expires_at,
                revoked_at = NULL
            WHERE reward_grants.status = 'revoked'
            RETURNING id
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(canonical_referrer_fan_id)
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
                "fan_id": canonical_referrer_fan_id,
                "qualified_referral_count": qualified_count,
                "threshold": threshold,
                "expires_at": crowdrelay_domain::wire_time::Wire(&expires_at),
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
                        workspace_id, reward_grant_id, code_hash, code_display,
                        discount_percent, max_uses, expires_at, status
                    )
                    SELECT $1, $2, digest(material.code, 'sha256'), material.code,
                           ($3::double precision)::numeric(5,2), 1, $5, 'issued'
                    FROM material
                    ON CONFLICT (workspace_id, reward_grant_id) DO UPDATE
                    SET code_hash = EXCLUDED.code_hash,
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
                        "fan_id": canonical_referrer_fan_id,
                        "email": &owner.normalized_email,
                        "display_name": &owner.display_name,
                        "coupon_code": &coupon.code_display,
                        "discount_percent": config.discount_percent,
                        "max_uses": 1,
                        "expires_at": crowdrelay_domain::wire_time::Wire(&expires_at),
                        "qualified_referral_count": qualified_count,
                    }),
                )
                .await?;
            }
            RewardConfig::PhysicalItem(config) => {
                append_outbox(
                    transaction,
                    workspace_id,
                    "physical_reward.granted",
                    request_id,
                    json!({
                        "workspace_id": workspace_id,
                        "reward_grant_id": grant_id,
                        "reward_rule_id": rule.id,
                        "fan_id": canonical_referrer_fan_id,
                        "email": &owner.normalized_email,
                        "display_name": &owner.display_name,
                        "item_name": config.item_name,
                        "sku": config.sku,
                        "expires_at": crowdrelay_domain::wire_time::Wire(&expires_at),
                        "qualified_referral_count": qualified_count,
                    }),
                )
                .await?;
            }
        }
    }

    Ok((qualified_count, revoked_grants, revoked_coupons))
}

/// Reconcile every referrer whose entitlement can change when two fan records
/// become one canonical person. This covers both directions: the merged person
/// may itself be a referrer, and it may be the referred person that makes two
/// previously separate attributions become ambiguous.
pub(crate) async fn reconcile_referral_rewards_after_identity_merge(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    canonical_fan_id: Uuid,
    request_id: &str,
) -> Result<(), ReferralStoreError> {
    let referrers = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT DISTINCT referrer_id
        FROM (
            SELECT canonical_fan_id($1,$2) AS referrer_id
            UNION ALL
            SELECT canonical_fan_id($1, attribution.referrer_fan_id)
            FROM referral_attributions attribution
            WHERE attribution.workspace_id = $1
              AND attribution.referred_fan_id IN (
                  SELECT fan_id FROM canonical_fan_family($1,$2)
              )
              AND attribution.status IN ('pending','qualified')
        ) impacted
        WHERE referrer_id IS NOT NULL
        ORDER BY referrer_id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(canonical_fan_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;

    for referrer in referrers {
        sync_referral_rewards_for_referrer(
            transaction,
            workspace_id,
            referrer,
            request_id,
        )
        .await?;
    }
    Ok(())
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
    let Some((canonical_referrer_fan_id, canonical_referred_fan_id)) =
        canonical_fan_pair(
            transaction,
            workspace_id,
            referrer_fan_id,
            referred_fan_id.into_uuid(),
        )
        .await?
    else {
        return Ok(());
    };

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
            WHERE workspace_id = $1 AND id = $5 AND status = 'active'
              AND deleted_at IS NULL
        )
        AND EXISTS (
            SELECT 1 FROM fans
            WHERE workspace_id = $1 AND id = $6 AND status = 'pending'
              AND deleted_at IS NULL
        )
        ON CONFLICT (workspace_id, referred_fan_id) DO NOTHING
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(referrer_fan_id)
    .bind(referred_fan_id.into_uuid())
    .bind(referral_code_id)
    .bind(canonical_referrer_fan_id)
    .bind(canonical_referred_fan_id)
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
    let Some((canonical_referrer_fan_id, canonical_referred_fan_id)) =
        canonical_fan_pair(
            transaction,
            workspace_id,
            referrer_fan_id,
            referred_fan_id.into_uuid(),
        )
        .await?
    else {
        return Ok(());
    };

    // Serialize qualification and threshold evaluation per canonical referrer. Without
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
    .bind(canonical_referrer_fan_id)
    .execute(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;

    let qualified = sqlx::query_as::<_, (Uuid, OffsetDateTime)>(
        r#"
        WITH promoted AS (
            UPDATE referral_attributions
               SET status = 'qualified',
                   qualification_reason = 'confirmed_fan_signup',
                   qualified_at = now()
             WHERE workspace_id = $1
               AND referrer_fan_id = $2
               AND referral_code_id = $4
               AND status = 'pending'
               AND referred_fan_id IN (
                   SELECT fan_id FROM canonical_fan_family($1,$3)
               )
               AND canonical_live_referral_owner_id($1,$3) = $5
               AND EXISTS (
                   SELECT 1 FROM fans
                    WHERE workspace_id=$1 AND id=$5
                      AND status='active' AND deleted_at IS NULL
               )
               AND EXISTS (
                   SELECT 1 FROM fans
                    WHERE workspace_id=$1 AND id=$6
                      AND status='active' AND deleted_at IS NULL
               )
            RETURNING id, qualified_at
        ), inserted AS (
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
            SELECT $1, $2, $3, $4, now(), 'qualified',
                   'active_fan_signup', now()
            WHERE NOT EXISTS (SELECT 1 FROM promoted)
              AND EXISTS (
                  SELECT 1 FROM fans
                   WHERE workspace_id=$1 AND id=$5
                     AND status='active' AND deleted_at IS NULL
              )
              AND EXISTS (
                  SELECT 1 FROM fans
                   WHERE workspace_id=$1 AND id=$6
                     AND status='active' AND deleted_at IS NULL
              )
              AND NOT EXISTS (
                  SELECT 1
                    FROM referral_attributions existing
                   WHERE existing.workspace_id=$1
                     AND existing.referred_fan_id IN (
                         SELECT fan_id FROM canonical_fan_family($1,$3)
                     )
                     AND existing.status IN ('pending','qualified')
              )
            ON CONFLICT (workspace_id, referred_fan_id) DO NOTHING
            RETURNING id, qualified_at
        )
        SELECT id, qualified_at FROM promoted
        UNION ALL
        SELECT id, qualified_at FROM inserted
        LIMIT 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(referrer_fan_id)
    .bind(referred_fan_id.into_uuid())
    .bind(referral_code_id)
    .bind(canonical_referrer_fan_id)
    .bind(canonical_referred_fan_id)
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
            "qualified_at": crowdrelay_domain::wire_time::Wire(&qualified_at),
        }),
    )
    .await?;

    sync_referral_rewards_for_referrer(
        transaction,
        workspace_id,
        canonical_referrer_fan_id,
        request_id,
    )
    .await?;
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
    let canonical_referred_fan_id = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT canonical_fan_id($1,$2)",
    )
    .bind(workspace_id.into_uuid())
    .bind(referred_fan_id.into_uuid())
    .fetch_one(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;
    let Some(canonical_referred_fan_id) = canonical_referred_fan_id else {
        return Ok(());
    };

    // Lock every qualified historical row for this one canonical human. A
    // duplicate merge can reveal more than one attribution; unsubscribing the
    // person reverses all of them, not whichever row happens to sort first.
    let attributions = sqlx::query_as::<_, (Uuid, Uuid, Option<Uuid>)>(
        r#"
        SELECT id, referrer_fan_id,
               canonical_fan_id($1, referrer_fan_id) AS canonical_referrer_fan_id
        FROM referral_attributions
        WHERE workspace_id = $1
          AND referred_fan_id IN (
              SELECT fan_id FROM canonical_fan_family($1,$2)
          )
          AND status = 'qualified'
        ORDER BY id
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(canonical_referred_fan_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;
    if attributions.is_empty() {
        return Ok(());
    }

    let mut canonical_referrers = attributions
        .iter()
        .filter_map(|row| row.2)
        .collect::<Vec<_>>();
    canonical_referrers.sort_unstable();
    canonical_referrers.dedup();

    // Acquire locks in UUID order so two concurrent family reversals cannot
    // deadlock by touching the same pair of referrers in opposite order.
    for referrer in &canonical_referrers {
        sqlx::query(
            "SELECT pg_advisory_xact_lock(
                 hashtextextended($1::uuid::text || ':' || $2::uuid::text, 0)
             )",
        )
        .bind(workspace_id.into_uuid())
        .bind(*referrer)
        .execute(&mut **transaction)
        .await
        .map_err(ReferralStoreError::from_sqlx)?;
    }

    let changed = sqlx::query(
        r#"
        UPDATE referral_attributions
        SET status = 'reversed',
            qualification_reason = 'fan_unsubscribed',
            reversed_at = now()
        WHERE workspace_id = $1
          AND referred_fan_id IN (
              SELECT fan_id FROM canonical_fan_family($1,$2)
          )
          AND status = 'qualified'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(canonical_referred_fan_id)
    .execute(&mut **transaction)
    .await
    .map_err(ReferralStoreError::from_sqlx)?;
    if changed.rows_affected() == 0 {
        return Ok(());
    }

    let mut synced = Vec::with_capacity(canonical_referrers.len());
    for referrer in canonical_referrers {
        let result = sync_referral_rewards_for_referrer(
            transaction,
            workspace_id,
            referrer,
            request_id,
        )
        .await?;
        synced.push((referrer, result));
    }

    for (attribution_id, historical_referrer_fan_id, canonical_referrer_fan_id) in attributions {
        let sync = canonical_referrer_fan_id
            .and_then(|referrer| {
                synced
                    .iter()
                    .find(|(candidate, _)| *candidate == referrer)
                    .map(|(_, result)| *result)
            })
            .unwrap_or((0, 0, 0));
        append_outbox(
            transaction,
            workspace_id,
            "referral.reversed",
            request_id,
            json!({
                "workspace_id": workspace_id,
                "attribution_id": attribution_id,
                "referrer_fan_id": historical_referrer_fan_id,
                "canonical_referrer_fan_id": canonical_referrer_fan_id,
                "referred_fan_id": referred_fan_id,
                "canonical_referred_fan_id": canonical_referred_fan_id,
                "qualified_referral_count": sync.0,
                "revoked_grant_count": sync.1,
                "revoked_coupon_count": sync.2,
            }),
        )
        .await?;
    }
    Ok(())
}
