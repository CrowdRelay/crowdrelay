async fn configure_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    statement_timeout_ms: i64,
    lock_timeout_ms: i64,
) -> Result<(), RetentionRunError> {
    sqlx::query(
        "SELECT set_config('statement_timeout', $1, true), set_config('lock_timeout', $2, true)",
    )
    .bind(format!("{statement_timeout_ms}ms"))
    .bind(format!("{lock_timeout_ms}ms"))
    .execute(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    Ok(())
}

async fn delete_expired_idempotency_keys(
    transaction: &mut Transaction<'_, Postgres>,
    batch_size: i64,
) -> Result<u64, RetentionRunError> {
    let result = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT idem.workspace_id, idem.scope, idem.key
            FROM idempotency_keys AS idem
            WHERE idem.expires_at <= now()
            ORDER BY idem.expires_at, idem.workspace_id, idem.scope, idem.key
            FOR UPDATE OF idem SKIP LOCKED
            LIMIT $1
        )
        DELETE FROM idempotency_keys AS idem
        USING candidates
        WHERE idem.workspace_id = candidates.workspace_id
            AND idem.scope = candidates.scope
            AND idem.key = candidates.key
        "#,
    )
    .bind(batch_size)
    .execute(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    Ok(result.rows_affected())
}

async fn delete_expired_webhook_replay_keys(
    transaction: &mut Transaction<'_, Postgres>,
    batch_size: i64,
) -> Result<u64, RetentionRunError> {
    let result = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT replay.workspace_id, replay.source, replay.event_id
            FROM webhook_replay_keys AS replay
            WHERE replay.expires_at <= now()
            ORDER BY replay.expires_at, replay.workspace_id, replay.source, replay.event_id
            FOR UPDATE OF replay SKIP LOCKED
            LIMIT $1
        )
        DELETE FROM webhook_replay_keys AS replay
        USING candidates
        WHERE replay.workspace_id = candidates.workspace_id
            AND replay.source = candidates.source
            AND replay.event_id = candidates.event_id
        "#,
    )
    .bind(batch_size)
    .execute(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    Ok(result.rows_affected())
}

async fn delete_terminal_fan_sessions(
    transaction: &mut Transaction<'_, Postgres>,
    batch_size: i64,
) -> Result<u64, RetentionRunError> {
    let result = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT session.id
            FROM fan_sessions AS session
            WHERE LEAST(
                session.expires_at,
                COALESCE(session.revoked_at, session.expires_at)
            ) <= now()
            ORDER BY
                LEAST(
                    session.expires_at,
                    COALESCE(session.revoked_at, session.expires_at)
                ),
                session.id
            FOR UPDATE OF session SKIP LOCKED
            LIMIT $1
        )
        DELETE FROM fan_sessions AS session
        USING candidates
        WHERE session.id = candidates.id
        "#,
    )
    .bind(batch_size)
    .execute(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    Ok(result.rows_affected())
}

async fn delete_terminal_pass_sessions(
    transaction: &mut Transaction<'_, Postgres>,
    batch_size: i64,
) -> Result<u64, RetentionRunError> {
    let result = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT session.id
            FROM pass_sessions AS session
            WHERE LEAST(
                session.expires_at,
                COALESCE(session.revoked_at, session.expires_at)
            ) <= now()
            ORDER BY
                LEAST(
                    session.expires_at,
                    COALESCE(session.revoked_at, session.expires_at)
                ),
                session.id
            FOR UPDATE OF session SKIP LOCKED
            LIMIT $1
        )
        DELETE FROM pass_sessions AS session
        USING candidates
        WHERE session.id = candidates.id
        "#,
    )
    .bind(batch_size)
    .execute(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    Ok(result.rows_affected())
}

async fn delete_terminal_member_sessions(
    transaction: &mut Transaction<'_, Postgres>,
    batch_size: i64,
) -> Result<u64, RetentionRunError> {
    let result = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT session.id
            FROM workspace_member_sessions AS session
            WHERE LEAST(
                session.expires_at,
                COALESCE(session.revoked_at, session.expires_at)
            ) <= now()
            AND NOT EXISTS (
                SELECT 1
                FROM pass_redemptions AS redemption
                WHERE redemption.workspace_id = session.workspace_id
                    AND redemption.staff_session_id = session.id
            )
            ORDER BY
                LEAST(
                    session.expires_at,
                    COALESCE(session.revoked_at, session.expires_at)
                ),
                session.id
            FOR UPDATE OF session SKIP LOCKED
            LIMIT $1
        )
        DELETE FROM workspace_member_sessions AS session
        USING candidates
        WHERE session.id = candidates.id
        "#,
    )
    .bind(batch_size)
    .execute(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    Ok(result.rows_affected())
}

async fn delete_terminal_fan_action_tokens(
    transaction: &mut Transaction<'_, Postgres>,
    batch_size: i64,
    consumed_token_retention_ms: i64,
) -> Result<u64, RetentionRunError> {
    let result = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT token.id
            FROM fan_action_tokens AS token
            WHERE token.expires_at <= now()
                OR token.consumed_at <=
                    now() - ($2::bigint * interval '1 millisecond')
            ORDER BY
                LEAST(
                    token.expires_at,
                    COALESCE(token.consumed_at, token.expires_at)
                ),
                token.id
            FOR UPDATE OF token SKIP LOCKED
            LIMIT $1
        )
        DELETE FROM fan_action_tokens AS token
        USING candidates
        WHERE token.id = candidates.id
        "#,
    )
    .bind(batch_size)
    .bind(consumed_token_retention_ms)
    .execute(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    Ok(result.rows_affected())
}

async fn reconcile_expired_admission_passes(
    transaction: &mut Transaction<'_, Postgres>,
    batch_size: i64,
) -> Result<u64, RetentionRunError> {
    let (expired_count, released_capacity) = sqlx::query_as::<_, (i64, i64)>(
        r#"
        WITH candidates AS (
            SELECT
                pass.id,
                pass.workspace_id,
                pass.admission_pool_id
            FROM admission_passes AS pass
            WHERE pass.status = 'issued'
                AND pass.claim_expires_at <= now()
            ORDER BY pass.claim_expires_at, pass.id
            FOR UPDATE OF pass SKIP LOCKED
            LIMIT $1
        ),
        expired AS (
            UPDATE admission_passes AS pass
            SET
                status = 'expired',
                claim_token_hash = NULL
            FROM candidates
            WHERE pass.workspace_id = candidates.workspace_id
                AND pass.id = candidates.id
                AND pass.status = 'issued'
                AND pass.claim_expires_at <= now()
            RETURNING pass.workspace_id, pass.admission_pool_id
        ),
        decrements AS (
            SELECT
                expired.workspace_id,
                expired.admission_pool_id,
                count(*)::bigint AS released_count
            FROM expired
            GROUP BY expired.workspace_id, expired.admission_pool_id
        ),
        updated_pools AS (
            UPDATE admission_pools AS pool
            SET issued_count =
                pool.issued_count - decrements.released_count::integer
            FROM decrements
            WHERE pool.workspace_id = decrements.workspace_id
                AND pool.id = decrements.admission_pool_id
                AND pool.issued_count >= decrements.released_count
            RETURNING decrements.released_count
        )
        SELECT
            (SELECT count(*)::bigint FROM expired),
            COALESCE(
                (SELECT sum(updated_pools.released_count)::bigint FROM updated_pools),
                0
            )
        "#,
    )
    .bind(batch_size)
    .fetch_one(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    if expired_count != released_capacity {
        return Err(RetentionRunError::Invariant);
    }
    u64::try_from(expired_count).map_err(|_| RetentionRunError::Invariant)
}

async fn delete_old_terminal_outbox_events(
    transaction: &mut Transaction<'_, Postgres>,
    batch_size: i64,
    terminal_outbox_retention_ms: i64,
) -> Result<u64, RetentionRunError> {
    // Deleting the parent cascades its terminal deliveries and attempt rows.
    // Standalone delivery deletion would break materialization idempotency while
    // the parent event is retained, so it is intentionally not performed.
    let result = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT event.id
            FROM outbox_events AS event
            WHERE event.status IN ('delivered', 'dead')
                AND COALESCE(event.delivered_at, event.dead_at) <=
                    now() - ($2::bigint * interval '1 millisecond')
                AND NOT EXISTS (
                    SELECT 1
                    FROM webhook_deliveries AS delivery
                    WHERE delivery.workspace_id = event.workspace_id
                        AND delivery.outbox_event_id = event.id
                        AND delivery.status IN ('pending', 'processing')
                )
            ORDER BY COALESCE(event.delivered_at, event.dead_at), event.id
            FOR UPDATE OF event SKIP LOCKED
            LIMIT $1
        )
        DELETE FROM outbox_events AS event
        USING candidates
        WHERE event.id = candidates.id
        "#,
    )
    .bind(batch_size)
    .bind(terminal_outbox_retention_ms)
    .execute(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    Ok(result.rows_affected())
}

async fn scrub_terminal_outbox_secrets(
    transaction: &mut Transaction<'_, Postgres>,
    batch_size: i64,
) -> Result<u64, RetentionRunError> {
    let result = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT event.id
            FROM outbox_events AS event
            WHERE event.status IN ('delivered', 'dead')
                AND event.payload ?| ARRAY[
                    'confirmation_token',
                    'session_recovery_token',
                    'unsubscribe_token',
                    'claim_token',
                    'coupon_code'
                ]
                AND NOT EXISTS (
                    SELECT 1
                    FROM webhook_deliveries AS delivery
                    WHERE delivery.workspace_id = event.workspace_id
                        AND delivery.outbox_event_id = event.id
                        AND delivery.status IN ('pending', 'processing')
                )
            ORDER BY COALESCE(event.delivered_at, event.dead_at), event.id
            FOR UPDATE OF event SKIP LOCKED
            LIMIT $1
        )
        UPDATE outbox_events AS event
        SET payload = event.payload - ARRAY[
            'confirmation_token',
            'session_recovery_token',
            'unsubscribe_token',
            'claim_token',
            'coupon_code'
        ]::text[]
        FROM candidates
        WHERE event.id = candidates.id
        "#,
    )
    .bind(batch_size)
    .execute(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    Ok(result.rows_affected())
}

async fn scrub_beacon_release_delivery_pii(
    transaction: &mut Transaction<'_, Postgres>,
    batch_size: i64,
) -> Result<u64, RetentionRunError> {
    let result = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT recipient.workspace_id,recipient.campaign_id,recipient.beacon_id
            FROM viryaos_beacon_release_recipients AS recipient
            WHERE recipient.status='delivered'
              AND recipient.pii_purged_at IS NULL
              AND recipient.delivery_details_purge_after IS NOT NULL
              AND recipient.delivery_details_purge_after <= now()
            ORDER BY recipient.delivery_details_purge_after,recipient.workspace_id,recipient.campaign_id,recipient.beacon_id
            FOR UPDATE OF recipient SKIP LOCKED
            LIMIT $1
        )
        UPDATE viryaos_beacon_release_recipients AS recipient
        SET recipient_name=NULL,recipient_phone=NULL,parcel_locker_code=NULL,
            pii_purged_at=now(),delivery_details_purge_after=NULL
        FROM candidates
        WHERE recipient.workspace_id=candidates.workspace_id
          AND recipient.campaign_id=candidates.campaign_id
          AND recipient.beacon_id=candidates.beacon_id
          AND recipient.status='delivered'
          AND recipient.pii_purged_at IS NULL
        "#,
    )
    .bind(batch_size)
    .execute(&mut **transaction)
    .await
    .map_err(RetentionRunError::Database)?;
    Ok(result.rows_affected())
}
