async fn load_candidates(
    transaction: &mut Transaction<'_, Postgres>,
    draw: &DrawRow,
) -> Result<Vec<CandidateRow>, DrawWorkerError> {
    sqlx::query_as::<_, CandidateRow>(
        r#"
        SELECT
            fan.id AS fan_id,
            fan.normalized_email,
            fan.display_name,
            referral_count.qualified_referrals,
            checkin_count.concert_checkins
        FROM fans AS fan
        CROSS JOIN LATERAL (
            SELECT count(*)::bigint AS qualified_referrals
            FROM referral_attributions AS attribution
            WHERE attribution.workspace_id = fan.workspace_id
              AND attribution.referrer_fan_id = fan.id
              AND attribution.status = 'qualified'
              AND attribution.qualified_at <= $4
        ) AS referral_count
        CROSS JOIN LATERAL (
            SELECT count(*)::bigint AS concert_checkins
            FROM concert_checkins AS checkin
            WHERE checkin.workspace_id = fan.workspace_id
              AND checkin.fan_id = fan.id
              AND checkin.checked_in_at >= $5
              AND checkin.checked_in_at <= $4
        ) AS checkin_count
        WHERE fan.workspace_id = $1
          AND fan.status <> 'suppressed'
          AND fan.created_at <= $4
          AND (
              (
                  $2 IN ('all_active', 'event_interest')
                  AND fan.status = 'active'
                  AND COALESCE(
                      (
                          SELECT max(token.consumed_at)
                          FROM fan_action_tokens AS token
                          WHERE token.workspace_id = fan.workspace_id
                            AND token.fan_id = fan.id
                            AND token.purpose = 'confirm'
                            AND token.consumed_at IS NOT NULL
                      ),
                      fan.created_at
                  ) <= $4
              )
              OR (
                  $2 = 'synesthesia_completion'
                  AND EXISTS (
                      SELECT 1
                      FROM synesthesia_reward_entries AS entry
                      WHERE entry.workspace_id = fan.workspace_id
                        AND entry.fan_id = fan.id
                        AND entry.campaign_slug = $6
                        AND entry.entered_at >= $5
                        AND entry.entered_at <= $4
                  )
              )
          )
          AND (
              $2 <> 'event_interest'
              OR EXISTS (
                  SELECT 1
                  FROM event_interests AS interest
                  WHERE interest.workspace_id = fan.workspace_id
                    AND interest.fan_id = fan.id
                    AND interest.event_id = $3
                    AND interest.created_at <= $4
              )
          )
        ORDER BY fan.id
        "#,
    )
    .bind(draw.workspace_id)
    .bind(&draw.eligibility_kind)
    .bind(draw.event_id)
    .bind(draw.closes_at)
    .bind(draw.opens_at)
    .bind(draw.eligibility_ref.as_deref())
    .fetch_all(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)
}
fn rank_candidates(
    candidates: Vec<CandidateRow>,
    draw: &DrawRow,
    seed: &[u8; 32],
) -> Result<Vec<RankedCandidate>, DrawWorkerError> {
    candidates
        .into_iter()
        .map(|candidate| {
            let qualified_referrals = i32::try_from(candidate.qualified_referrals)
                .map_err(|_| DrawWorkerError::Arithmetic)?;
            let available_bonus_entries = draw
                .max_entries
                .checked_sub(draw.base_entries)
                .ok_or(DrawWorkerError::Arithmetic)?;
            let referral_entries = qualified_referrals
                .checked_mul(draw.entries_per_referral)
                .ok_or(DrawWorkerError::Arithmetic)?
                .min(available_bonus_entries);
            let concert_checkins = i32::try_from(candidate.concert_checkins)
                .map_err(|_| DrawWorkerError::Arithmetic)?;
            let remaining_entries = available_bonus_entries
                .checked_sub(referral_entries)
                .ok_or(DrawWorkerError::Arithmetic)?;
            let checkin_entries = concert_checkins
                .checked_mul(draw.entries_per_checkin)
                .ok_or(DrawWorkerError::Arithmetic)?
                .min(remaining_entries);
            let entry_count = draw
                .base_entries
                .checked_add(referral_entries)
                .and_then(|value| value.checked_add(checkin_entries))
                .ok_or(DrawWorkerError::Arithmetic)?;
            let selection_score = weighted_score(seed, candidate.fan_id, entry_count)?;
            Ok(RankedCandidate {
                fan_id: candidate.fan_id,
                normalized_email: candidate.normalized_email,
                display_name: candidate.display_name,
                qualified_referrals,
                concert_checkins,
                checkin_entries,
                entry_count,
                selection_score,
            })
        })
        .collect()
}

#[allow(clippy::cast_precision_loss)]
fn weighted_score(seed: &[u8; 32], fan_id: Uuid, entry_count: i32) -> Result<f64, DrawWorkerError> {
    if entry_count <= 0 {
        return Err(DrawWorkerError::InvalidDraw);
    }
    let mut mac = HmacSha256::new_from_slice(seed).map_err(|_| DrawWorkerError::Entropy)?;
    mac.update(fan_id.as_bytes());
    let digest = mac.finalize().into_bytes();
    let bytes: [u8; 8] = digest
        .get(..8)
        .ok_or(DrawWorkerError::Entropy)?
        .try_into()
        .map_err(|_| DrawWorkerError::Entropy)?;
    let random = u64::from_be_bytes(bytes);
    // Use the 53 significant bits representable by f64. The half-step keeps
    // the value strictly inside (0, 1), so the exponential-race score can
    // never become zero or infinity at the database boundary.
    let mantissa = random >> 11;
    let unit = (mantissa as f64 + 0.5) / 9_007_199_254_740_992.0;
    Ok(-unit.ln() / f64::from(entry_count))
}

fn compare_candidates(left: &RankedCandidate, right: &RankedCandidate) -> Ordering {
    left.selection_score
        .total_cmp(&right.selection_score)
        .then_with(|| left.fan_id.cmp(&right.fan_id))
}

async fn persist_candidates(
    transaction: &mut Transaction<'_, Postgres>,
    draw: &DrawRow,
    run_id: Uuid,
    candidates: &[RankedCandidate],
) -> Result<(), DrawWorkerError> {
    if candidates.is_empty() {
        return Ok(());
    }

    let fan_ids: Vec<Uuid> = candidates
        .iter()
        .map(|candidate| candidate.fan_id)
        .collect();
    let qualified_referrals: Vec<i32> = candidates
        .iter()
        .map(|candidate| candidate.qualified_referrals)
        .collect();
    let concert_checkins: Vec<i32> = candidates
        .iter()
        .map(|candidate| candidate.concert_checkins)
        .collect();
    let checkin_entries: Vec<i32> = candidates
        .iter()
        .map(|candidate| candidate.checkin_entries)
        .collect();
    let entry_counts: Vec<i32> = candidates
        .iter()
        .map(|candidate| candidate.entry_count)
        .collect();
    let selection_scores: Vec<f64> = candidates
        .iter()
        .map(|candidate| candidate.selection_score)
        .collect();

    sqlx::query(
        r#"
        INSERT INTO reward_draw_candidates (
            workspace_id, draw_id, run_id, fan_id, qualified_referrals,
            concert_checkins, checkin_entries, entry_count, selection_score
        )
        SELECT $1, $2, $3, candidate.*
        FROM unnest(
            $4::uuid[],
            $5::integer[],
            $6::integer[],
            $7::integer[],
            $8::integer[],
            $9::double precision[]
        ) AS candidate(
            fan_id, qualified_referrals, concert_checkins,
            checkin_entries, entry_count, selection_score
        )
        "#,
    )
    .bind(draw.workspace_id)
    .bind(draw.id)
    .bind(run_id)
    .bind(fan_ids)
    .bind(qualified_referrals)
    .bind(concert_checkins)
    .bind(checkin_entries)
    .bind(entry_counts)
    .bind(selection_scores)
    .execute(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;
    Ok(())
}

async fn issue_admission_winners(
    transaction: &mut Transaction<'_, Postgres>,
    draw: &DrawRow,
    run_id: Uuid,
    candidates: &[RankedCandidate],
) -> Result<usize, DrawWorkerError> {
    let event_id = draw.event_id.ok_or(DrawWorkerError::InvalidDraw)?;
    let pool_id = draw.admission_pool_id.ok_or(DrawWorkerError::InvalidDraw)?;
    let pool = sqlx::query_as::<_, AdmissionPoolRow>(
        r#"
        SELECT id, capacity, issued_count, reserved_count
        FROM admission_pools
        WHERE workspace_id = $1 AND id = $2 AND event_id = $3 AND active
        FOR UPDATE
        "#,
    )
    .bind(draw.workspace_id)
    .bind(pool_id)
    .bind(event_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?
    .ok_or(DrawWorkerError::InvalidDraw)?;

    let available = pool
        .capacity
        .saturating_sub(pool.issued_count)
        .saturating_sub(pool.reserved_count);
    let target = usize::try_from(draw.winner_count.min(available).max(0))
        .map_err(|_| DrawWorkerError::Arithmetic)?;
    let mut selected = 0_usize;

    for candidate in candidates {
        if selected >= target {
            break;
        }
        // No pre-flight EXISTS here: the pass insert below carries a unique
        // constraint on (workspace_id, admission_pool_id, fan_id) with
        // ON CONFLICT DO NOTHING, so a repeat winner costs one statement,
        // not two.
        let mut token_bytes = [0_u8; 32];
        fill_random(&mut token_bytes).map_err(|_| DrawWorkerError::Entropy)?;
        let claim_token = hex::encode(token_bytes);
        let reference_bytes = token_bytes.get(..6).ok_or(DrawWorkerError::Entropy)?;
        let public_reference = format!("VIRYA-{}", hex::encode(reference_bytes).to_uppercase());
        let pass_id = Uuid::now_v7();
        let claim_expires_at = OffsetDateTime::now_utc()
            .checked_add(time::Duration::hours(i64::from(draw.claim_expires_hours)))
            .ok_or(DrawWorkerError::Arithmetic)?;

        let inserted = sqlx::query(
            r#"
            INSERT INTO admission_passes (
                id, workspace_id, event_id, admission_pool_id, fan_id,
                issuance_method, public_reference, claim_token_hash,
                claim_expires_at, status
            )
            VALUES ($1, $2, $3, $4, $5, 'weighted_draw', $6, digest($7, 'sha256'), $8, 'issued')
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(pass_id)
        .bind(draw.workspace_id)
        .bind(event_id)
        .bind(pool.id)
        .bind(candidate.fan_id)
        .bind(&public_reference)
        .bind(&claim_token)
        .bind(claim_expires_at)
        .execute(&mut **transaction)
        .await
        .map_err(DrawWorkerError::sqlx)?;

        if inserted.rows_affected() == 0 {
            continue;
        }

        selected = selected.saturating_add(1);
        let winner_rank = i32::try_from(selected).map_err(|_| DrawWorkerError::Arithmetic)?;
        let _winner_id = record_winner(
            transaction,
            draw,
            run_id,
            candidate,
            winner_rank,
            Some(pass_id),
            None,
        )
        .await?;

        append_outbox(
            transaction,
            draw.workspace_id,
            "admission.pass.issued",
            &format!("draw:{}:fan:{}", draw.id, candidate.fan_id),
            json!({
                "pass_id": pass_id,
                "event_id": event_id,
                "admission_pool_id": pool.id,
                "fan_id": candidate.fan_id,
                "email": candidate.normalized_email,
                "display_name": candidate.display_name,
                "public_reference": public_reference,
                "claim_token": claim_token,
                "claim_expires_at": claim_expires_at,
                "issuance_method": "weighted_draw",
                "draw_id": draw.id,
                "draw_slug": draw.slug,
                "winner_rank": winner_rank,
                "entry_count": candidate.entry_count,
                "qualified_referrals": candidate.qualified_referrals,
                "concert_checkins": candidate.concert_checkins,
                "checkin_entries": candidate.checkin_entries,
            }),
        )
        .await?;
    }

    sqlx::query(
        "UPDATE admission_pools SET issued_count = issued_count + $3 WHERE workspace_id = $1 AND id = $2",
    )
    .bind(draw.workspace_id)
    .bind(pool.id)
    .bind(i32::try_from(selected).map_err(|_| DrawWorkerError::Arithmetic)?)
    .execute(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;

    Ok(selected)
}

async fn issue_physical_winners(
    transaction: &mut Transaction<'_, Postgres>,
    draw: &DrawRow,
    run_id: Uuid,
    candidates: &[RankedCandidate],
) -> Result<usize, DrawWorkerError> {
    let reward_rule_id = draw.reward_rule_id.ok_or(DrawWorkerError::InvalidDraw)?;
    let rule = sqlx::query_as::<_, PhysicalRuleRow>(
        r#"
        SELECT
            id,
            config->>'item_name' AS item_name,
            config->>'sku' AS sku,
            COALESCE((config->>'expires_days')::integer, 365) AS expires_days
        FROM reward_rules
        WHERE workspace_id = $1
          AND id = $2
          AND reward_type = 'physical_item'
          AND active
          AND btrim(COALESCE(config->>'item_name', '')) <> ''
          AND btrim(COALESCE(config->>'sku', '')) <> ''
          AND COALESCE((config->>'expires_days')::integer, 365) BETWEEN 1 AND 3650
        FOR SHARE
        "#,
    )
    .bind(draw.workspace_id)
    .bind(reward_rule_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?
    .ok_or(DrawWorkerError::InvalidDraw)?;

    let target = usize::try_from(draw.winner_count).map_err(|_| DrawWorkerError::Arithmetic)?;
    let mut selected = 0_usize;
    for candidate in candidates {
        if selected >= target {
            break;
        }
        let grant_id = Uuid::now_v7();
        let qualification_key = format!("weighted_draw:{}", draw.id);
        let inserted = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO reward_grants (
                id, workspace_id, fan_id, reward_rule_id, qualification_key,
                status, issued_at, expires_at
            )
            VALUES ($1, $2, $3, $4, $5, 'issued', now(), now() + ($6::bigint * interval '1 day'))
            ON CONFLICT (workspace_id, reward_rule_id, fan_id, qualification_key) DO NOTHING
            RETURNING id
            "#,
        )
        .bind(grant_id)
        .bind(draw.workspace_id)
        .bind(candidate.fan_id)
        .bind(rule.id)
        .bind(&qualification_key)
        .bind(rule.expires_days)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(DrawWorkerError::sqlx)?;

        let Some(grant_id) = inserted else {
            continue;
        };

        selected = selected.saturating_add(1);
        let winner_rank = i32::try_from(selected).map_err(|_| DrawWorkerError::Arithmetic)?;
        let winner_id = record_winner(
            transaction,
            draw,
            run_id,
            candidate,
            winner_rank,
            None,
            Some(grant_id),
        )
        .await?;

        sqlx::query(
            r#"
            INSERT INTO reward_draw_fulfillments (
                workspace_id, draw_id, winner_id, reward_grant_id,
                variant_id, quantity, status
            )
            SELECT
                allocation.workspace_id, allocation.draw_id, $3, $4,
                allocation.variant_id, allocation.units_per_winner, 'pending'
            FROM reward_draw_inventory_allocations AS allocation
            WHERE allocation.workspace_id = $1 AND allocation.draw_id = $2
            ON CONFLICT (workspace_id, winner_id) DO NOTHING
            "#,
        )
        .bind(draw.workspace_id)
        .bind(draw.id)
        .bind(winner_id)
        .bind(grant_id)
        .execute(&mut **transaction)
        .await
        .map_err(DrawWorkerError::sqlx)?;

        append_outbox(
            transaction,
            draw.workspace_id,
            "physical_reward.granted",
            &format!("draw:{}:fan:{}", draw.id, candidate.fan_id),
            json!({
                "workspace_id": draw.workspace_id,
                "reward_grant_id": grant_id,
                "reward_rule_id": rule.id,
                "fan_id": candidate.fan_id,
                "email": candidate.normalized_email,
                "display_name": candidate.display_name,
                "item_name": rule.item_name,
                "sku": rule.sku,
                "draw_id": draw.id,
                "draw_slug": draw.slug,
                "winner_rank": winner_rank,
                "entry_count": candidate.entry_count,
                "qualified_referrals": candidate.qualified_referrals,
                "concert_checkins": candidate.concert_checkins,
                "checkin_entries": candidate.checkin_entries,
            }),
        )
        .await?;
    }
    reconcile_physical_inventory_reservation(transaction, draw, selected).await?;
    Ok(selected)
}

async fn reconcile_physical_inventory_reservation(
    transaction: &mut Transaction<'_, Postgres>,
    draw: &DrawRow,
    selected: usize,
) -> Result<(), DrawWorkerError> {
    let selected = i32::try_from(selected).map_err(|_| DrawWorkerError::Arithmetic)?;
    let allocation = sqlx::query_as::<_, (Uuid, Uuid, i32)>(
        r#"
        SELECT reservation_id, variant_id, units_per_winner
        FROM reward_draw_inventory_allocations
        WHERE workspace_id = $1 AND draw_id = $2
        FOR UPDATE
        "#,
    )
    .bind(draw.workspace_id)
    .bind(draw.id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;
    let Some((reservation_id, variant_id, units_per_winner)) = allocation else {
        return Ok(());
    };
    let required = selected
        .checked_mul(units_per_winner)
        .ok_or(DrawWorkerError::Arithmetic)?;
    if required == 0 {
        sqlx::query(
            r#"
            DELETE FROM inventory_reservation_items
            WHERE workspace_id = $1 AND reservation_id = $2 AND variant_id = $3
            "#,
        )
        .bind(draw.workspace_id)
        .bind(reservation_id)
        .bind(variant_id)
        .execute(&mut **transaction)
        .await
        .map_err(DrawWorkerError::sqlx)?;
        sqlx::query(
            r#"
            UPDATE inventory_reservations
            SET status = 'released', released_at = now(),
                release_reason = 'draw completed without winners'
            WHERE workspace_id = $1 AND id = $2 AND status = 'active'
            "#,
        )
        .bind(draw.workspace_id)
        .bind(reservation_id)
        .execute(&mut **transaction)
        .await
        .map_err(DrawWorkerError::sqlx)?;
    } else {
        sqlx::query(
            r#"
            UPDATE inventory_reservation_items
            SET quantity = LEAST(quantity, $4)
            WHERE workspace_id = $1 AND reservation_id = $2 AND variant_id = $3
            "#,
        )
        .bind(draw.workspace_id)
        .bind(reservation_id)
        .bind(variant_id)
        .bind(required)
        .execute(&mut **transaction)
        .await
        .map_err(DrawWorkerError::sqlx)?;
    }
    Ok(())
}

async fn record_winner(
    transaction: &mut Transaction<'_, Postgres>,
    draw: &DrawRow,
    run_id: Uuid,
    candidate: &RankedCandidate,
    winner_rank: i32,
    admission_pass_id: Option<Uuid>,
    reward_grant_id: Option<Uuid>,
) -> Result<Uuid, DrawWorkerError> {
    let winner_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO reward_draw_winners (
            id, workspace_id, draw_id, run_id, fan_id, winner_rank,
            admission_pass_id, reward_grant_id
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
    )
    .bind(winner_id)
    .bind(draw.workspace_id)
    .bind(draw.id)
    .bind(run_id)
    .bind(candidate.fan_id)
    .bind(winner_rank)
    .bind(admission_pass_id)
    .bind(reward_grant_id)
    .execute(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;

    sqlx::query(
        r#"
        UPDATE reward_draw_candidates
        SET selected = true, winner_rank = $4
        WHERE workspace_id = $1 AND run_id = $2 AND fan_id = $3
        "#,
    )
    .bind(draw.workspace_id)
    .bind(run_id)
    .bind(candidate.fan_id)
    .bind(winner_rank)
    .execute(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;

    append_outbox(
        transaction,
        draw.workspace_id,
        "reward_draw.winner",
        &format!("draw:{}:winner:{}", draw.id, candidate.fan_id),
        json!({
            "draw_id": draw.id,
            "draw_slug": draw.slug,
            "draw_name": draw.name,
            "run_id": run_id,
            "fan_id": candidate.fan_id,
            "email": candidate.normalized_email,
            "display_name": candidate.display_name,
            "winner_rank": winner_rank,
            "entry_count": candidate.entry_count,
            "qualified_referrals": candidate.qualified_referrals,
            "concert_checkins": candidate.concert_checkins,
            "checkin_entries": candidate.checkin_entries,
            "prize_kind": draw.prize_kind,
            "admission_pass_id": admission_pass_id,
            "reward_grant_id": reward_grant_id,
        }),
    )
    .await?;
    Ok(winner_id)
}

async fn append_outbox(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    event_type: &str,
    request_id: &str,
    payload: serde_json::Value,
) -> Result<(), DrawWorkerError> {
    sqlx::query(
        "INSERT INTO outbox_events (workspace_id, event_type, event_version, payload, request_id) VALUES ($1, $2, 1, $3, $4)",
    )
    .bind(workspace_id)
    .bind(event_type)
    .bind(payload)
    .bind(request_id)
    .execute(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;
    Ok(())
}

async fn append_audit(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    action: &str,
    target_type: &str,
    target_id: Uuid,
    metadata: serde_json::Value,
) -> Result<(), DrawWorkerError> {
    sqlx::query(
        r#"
        INSERT INTO audit_events (
            workspace_id, actor_kind, action, target_type, target_id, metadata
        )
        VALUES ($1, 'system', $2, $3, $4, $5)
        "#,
    )
    .bind(workspace_id)
    .bind(action)
    .bind(target_type)
    .bind(target_id.to_string())
    .bind(metadata)
    .execute(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;
    Ok(())
}

async fn record_failure(
    pool: &PgPool,
    draw_id: Uuid,
    error: &DrawWorkerError,
    config: &WeightedDrawWorkerConfig,
) -> Result<(), DrawWorkerError> {
    let message = truncate_error(&error.to_string());
    timeout(
        config.operation_timeout,
        sqlx::query(
            r#"
            UPDATE reward_draws
            SET attempts = attempts + 1,
                status = CASE WHEN attempts + 1 >= 10 THEN 'cancelled' ELSE 'scheduled' END,
                draw_at = CASE
                    WHEN attempts + 1 >= 10 THEN draw_at
                    ELSE now() + interval '5 minutes'
                END,
                last_error = $2
            WHERE id = $1 AND status IN ('scheduled', 'running')
            "#,
        )
        .bind(draw_id)
        .bind(message)
        .execute(pool),
    )
    .await
    .map_err(|_| DrawWorkerError::TimedOut)?
    .map_err(DrawWorkerError::sqlx)?;
    Ok(())
}
