#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use sqlx::{PgPool, postgres::PgPoolOptions};
    use uuid::Uuid;

    use super::{
        MAX_BATCH_SIZE, RetentionStats, RetentionWorker, RetentionWorkerBuildError,
        RetentionWorkerConfig,
    };

    fn lazy_pool() -> Result<PgPool, sqlx::Error> {
        PgPoolOptions::new().connect_lazy("postgres://crowdrelay:crowdrelay@localhost/crowdrelay")
    }

    #[tokio::test]
    async fn default_configuration_is_valid_and_bounded() -> Result<(), Box<dyn std::error::Error>>
    {
        RetentionWorker::new(lazy_pool()?, RetentionWorkerConfig::default())?;
        RetentionWorker::new(
            lazy_pool()?,
            RetentionWorkerConfig {
                batch_size: MAX_BATCH_SIZE,
                ..RetentionWorkerConfig::default()
            },
        )?;
        Ok(())
    }

    #[tokio::test]
    async fn rejects_zero_durations() -> Result<(), Box<dyn std::error::Error>> {
        let defaults = RetentionWorkerConfig::default();
        for config in [
            RetentionWorkerConfig {
                poll_interval: Duration::ZERO,
                ..defaults
            },
            RetentionWorkerConfig {
                operation_timeout: Duration::ZERO,
                ..defaults
            },
            RetentionWorkerConfig {
                lock_timeout: Duration::ZERO,
                ..defaults
            },
            RetentionWorkerConfig {
                terminal_outbox_retention: Duration::ZERO,
                ..defaults
            },
            RetentionWorkerConfig {
                consumed_token_retention: Duration::ZERO,
                ..defaults
            },
        ] {
            assert_eq!(
                RetentionWorker::new(lazy_pool()?, config).expect_err("config must be rejected"),
                RetentionWorkerBuildError::ZeroDuration
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn rejects_unbounded_batch_and_invalid_timeout_order()
    -> Result<(), Box<dyn std::error::Error>> {
        for batch_size in [0, MAX_BATCH_SIZE + 1] {
            let error = RetentionWorker::new(
                lazy_pool()?,
                RetentionWorkerConfig {
                    batch_size,
                    ..RetentionWorkerConfig::default()
                },
            )
            .expect_err("batch must be rejected");
            assert_eq!(error, RetentionWorkerBuildError::InvalidBatchSize);
        }

        let error = RetentionWorker::new(
            lazy_pool()?,
            RetentionWorkerConfig {
                operation_timeout: Duration::from_secs(1),
                lock_timeout: Duration::from_secs(2),
                ..RetentionWorkerConfig::default()
            },
        )
        .expect_err("timeout order must be rejected");
        assert_eq!(error, RetentionWorkerBuildError::InvalidTimeoutOrder);
        Ok(())
    }

    #[tokio::test]
    async fn rejects_duration_that_postgres_cannot_represent()
    -> Result<(), Box<dyn std::error::Error>> {
        let error = RetentionWorker::new(
            lazy_pool()?,
            RetentionWorkerConfig {
                terminal_outbox_retention: Duration::MAX,
                ..RetentionWorkerConfig::default()
            },
        )
        .expect_err("overflow must be rejected");
        assert_eq!(error, RetentionWorkerBuildError::DurationOverflow);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires CROWDRELAY_RETENTION_TEST_DATABASE_URL and a disposable PostgreSQL database"]
    async fn cycle_deletes_expired_rows_scrubs_safe_payloads_and_preserves_audit()
    -> Result<(), Box<dyn std::error::Error>> {
        let database_url =
            std::env::var("CROWDRELAY_RETENTION_TEST_DATABASE_URL").map_err(|e| {
                format!(
                    "CROWDRELAY_RETENTION_TEST_DATABASE_URL must target a disposable database: {e}"
                )
            })?;
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await?;
        crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

        let suffix = Uuid::now_v7().simple().to_string();
        let workspace_id = Uuid::now_v7();
        let fan_id = Uuid::now_v7();
        let event_id = Uuid::now_v7();
        let admission_pool_id = Uuid::now_v7();
        let expired_pass_id = Uuid::now_v7();
        let expired_session_id = Uuid::now_v7();
        let expired_action_id = Uuid::now_v7();
        let recent_consumed_action_id = Uuid::now_v7();
        let endpoint_id = Uuid::now_v7();
        let scrubbed_event_id = Uuid::now_v7();
        let blocked_event_id = Uuid::now_v7();
        let deleted_event_id = Uuid::now_v7();
        let blocked_delivery_id = Uuid::now_v7();
        let deleted_delivery_id = Uuid::now_v7();
        let expired_idempotency_key = format!("expired-{suffix}");
        let retained_idempotency_key = format!("retained-{suffix}");
        let expired_replay_id = format!("expired-replay-{suffix}");

        sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Retention test')")
            .bind(workspace_id)
            .bind(format!("retention-{suffix}"))
            .execute(&pool)
            .await?;
        sqlx::query(
            r#"
            INSERT INTO fans (
                id, workspace_id, normalized_email, display_name, locale, status
            )
            VALUES ($1, $2, $3, 'Retention fan', 'pl-PL', 'active')
            "#,
        )
        .bind(fan_id)
        .bind(workspace_id)
        .bind(format!("retention-{suffix}@example.test"))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO fan_consents (
                workspace_id, fan_id, purpose, granted, policy_version, source
            )
            VALUES ($1, $2, 'marketing', true, 'retention-v1', 'retention-test')
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO audit_events (
                workspace_id, actor_kind, action, target_type, target_id
            )
            VALUES ($1, 'system', 'retention.test', 'fan', $2)
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id.to_string())
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO events (
                id, workspace_id, slug, title, starts_at, status, published_at
            )
            VALUES (
                $1, $2, $3, 'Retention event',
                now() + interval '30 days', 'published', now()
            )
            "#,
        )
        .bind(event_id)
        .bind(workspace_id)
        .bind(format!("retention-event-{suffix}"))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO admission_pools (
                id, workspace_id, event_id, name, slug, capacity, issued_count
            )
            VALUES ($1, $2, $3, 'Retention pool', $4, 10, 1)
            "#,
        )
        .bind(admission_pool_id)
        .bind(workspace_id)
        .bind(event_id)
        .bind(format!("retention-pool-{suffix}"))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO admission_passes (
                id, workspace_id, event_id, admission_pool_id, fan_id,
                issuance_method, public_reference, claim_token_hash,
                claim_expires_at, status, issued_at
            )
            VALUES (
                $1, $2, $3, $4, $5,
                'first_come', $6, digest($7, 'sha256'),
                now() - interval '1 day', 'issued',
                now() - interval '2 days'
            )
            "#,
        )
        .bind(expired_pass_id)
        .bind(workspace_id)
        .bind(event_id)
        .bind(admission_pool_id)
        .bind(fan_id)
        .bind(format!("RETENTION-{suffix}"))
        .bind(format!("expired-claim-{suffix}"))
        .execute(&pool)
        .await?;

        sqlx::query(
            r#"
            INSERT INTO idempotency_keys (
                workspace_id, scope, key, request_hash, state, response_status,
                response_body, response_content_type, created_at, completed_at, expires_at
            )
            VALUES (
                $1, 'retention-test', $2, digest($3, 'sha256'), 'completed', 200,
                '{}'::jsonb, 'application/json',
                now() - interval '3 days',
                now() - interval '2 days',
                now() - interval '1 day'
            )
            "#,
        )
        .bind(workspace_id)
        .bind(&expired_idempotency_key)
        .bind(format!("request-expired-{suffix}"))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO idempotency_keys (
                workspace_id, scope, key, request_hash, state, response_status,
                response_body, response_content_type, completed_at, expires_at
            )
            VALUES (
                $1, 'retention-test', $2, digest($3, 'sha256'), 'completed', 200,
                '{}'::jsonb, 'application/json', now(), now() + interval '1 day'
            )
            "#,
        )
        .bind(workspace_id)
        .bind(&retained_idempotency_key)
        .bind(format!("request-retained-{suffix}"))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO webhook_replay_keys (
                workspace_id, source, event_id, body_sha256,
                signed_at, received_at, expires_at
            )
            VALUES (
                $1, 'retention-test', $2, digest($3, 'sha256'),
                now() - interval '2 days',
                now() - interval '2 days',
                now() - interval '1 day'
            )
            "#,
        )
        .bind(workspace_id)
        .bind(&expired_replay_id)
        .bind(format!("replay-body-{suffix}"))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO fan_sessions (
                id, workspace_id, fan_id, session_token_hash,
                created_at, last_seen_at, expires_at
            )
            VALUES (
                $1, $2, $3, digest($4, 'sha256'),
                now() - interval '2 days',
                now() - interval '2 days',
                now() - interval '1 day'
            )
            "#,
        )
        .bind(expired_session_id)
        .bind(workspace_id)
        .bind(fan_id)
        .bind(format!("expired-session-{suffix}"))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO fan_action_tokens (
                id, workspace_id, fan_id, purpose, token_hash,
                created_at, expires_at
            )
            VALUES (
                $1, $2, $3, 'confirm', digest($4, 'sha256'),
                now() - interval '2 days',
                now() - interval '1 day'
            )
            "#,
        )
        .bind(expired_action_id)
        .bind(workspace_id)
        .bind(fan_id)
        .bind(format!("expired-action-{suffix}"))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO fan_action_tokens (
                id, workspace_id, fan_id, purpose, token_hash,
                created_at, expires_at, consumed_at
            )
            VALUES (
                $1, $2, $3, 'unsubscribe', digest($4, 'sha256'),
                now() - interval '1 day',
                now() + interval '30 days',
                now()
            )
            "#,
        )
        .bind(recent_consumed_action_id)
        .bind(workspace_id)
        .bind(fan_id)
        .bind(format!("recent-action-{suffix}"))
        .execute(&pool)
        .await?;

        sqlx::query(
            r#"
            INSERT INTO webhook_endpoints (
                id, workspace_id, name, url, signing_secret_ref
            )
            VALUES ($1, $2, 'Retention endpoint', 'https://example.test/hook', 'retention-secret')
            "#,
        )
        .bind(endpoint_id)
        .bind(workspace_id)
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO outbox_events (
                id, workspace_id, event_type, payload, status, delivered_at
            )
            VALUES ($1, $2, 'fan.confirmed', $3, 'delivered', now())
            "#,
        )
        .bind(scrubbed_event_id)
        .bind(workspace_id)
        .bind(json!({
            "email": "fan@example.test",
            "unsubscribe_token": "remove-me",
            "confirmation_token": "remove-me-too"
        }))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO outbox_events (
                id, workspace_id, event_type, payload, status,
                created_at, delivered_at
            )
            VALUES (
                $1, $2, 'admission.pass.issued', $3, 'delivered',
                now() - interval '32 days',
                now() - interval '31 days'
            )
            "#,
        )
        .bind(blocked_event_id)
        .bind(workspace_id)
        .bind(json!({"claim_token": "still-needed"}))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO webhook_deliveries (
                id, workspace_id, outbox_event_id, endpoint_id,
                status, max_attempts, created_at
            )
            VALUES (
                $1, $2, $3, $4, 'pending', 3,
                now() - interval '31 days'
            )
            "#,
        )
        .bind(blocked_delivery_id)
        .bind(workspace_id)
        .bind(blocked_event_id)
        .bind(endpoint_id)
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO outbox_events (
                id, workspace_id, event_type, payload, status,
                created_at, delivered_at
            )
            VALUES (
                $1, $2, 'merch_coupon.issued', $3, 'delivered',
                now() - interval '32 days',
                now() - interval '31 days'
            )
            "#,
        )
        .bind(deleted_event_id)
        .bind(workspace_id)
        .bind(json!({"coupon_code": "delete-with-parent"}))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO webhook_deliveries (
                id, workspace_id, outbox_event_id, endpoint_id,
                status, attempt_count, max_attempts,
                created_at, delivered_at
            )
            VALUES (
                $1, $2, $3, $4, 'delivered', 1, 3,
                now() - interval '32 days',
                now() - interval '31 days'
            )
            "#,
        )
        .bind(deleted_delivery_id)
        .bind(workspace_id)
        .bind(deleted_event_id)
        .bind(endpoint_id)
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO webhook_delivery_attempts (
                workspace_id, delivery_id, attempt_number,
                started_at, finished_at, outcome,
                response_status, duration_ms
            )
            VALUES (
                $1, $2, 1,
                now() - interval '31 days' - interval '1 second',
                now() - interval '31 days',
                'delivered', 204, 1000
            )
            "#,
        )
        .bind(workspace_id)
        .bind(deleted_delivery_id)
        .execute(&pool)
        .await?;

        let worker = RetentionWorker::new(
            pool.clone(),
            RetentionWorkerConfig {
                operation_timeout: Duration::from_secs(5),
                lock_timeout: Duration::from_secs(1),
                ..RetentionWorkerConfig::default()
            },
        )?;
        let stats = worker.run_once().await?;
        assert!(stats.idempotency_keys_deleted >= 1);
        assert!(stats.webhook_replay_keys_deleted >= 1);
        assert!(stats.fan_sessions_deleted >= 1);
        assert!(stats.fan_action_tokens_deleted >= 1);
        assert_eq!(stats.expired_admission_passes_reconciled, 1);
        assert!(stats.outbox_payloads_scrubbed >= 1);
        assert!(stats.terminal_outbox_events_deleted >= 1);

        assert!(
            !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                    SELECT 1 FROM idempotency_keys
                    WHERE workspace_id = $1 AND scope = 'retention-test' AND key = $2
                )",
            )
            .bind(workspace_id)
            .bind(&expired_idempotency_key)
            .fetch_one(&pool)
            .await?
        );
        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                    SELECT 1 FROM idempotency_keys
                    WHERE workspace_id = $1 AND scope = 'retention-test' AND key = $2
                )",
            )
            .bind(workspace_id)
            .bind(&retained_idempotency_key)
            .fetch_one(&pool)
            .await?
        );
        let (pass_status, claim_token_hash) = sqlx::query_as::<_, (String, Option<Vec<u8>>)>(
            "SELECT status, claim_token_hash FROM admission_passes WHERE id = $1",
        )
        .bind(expired_pass_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(pass_status, "expired");
        assert_eq!(claim_token_hash, None);
        assert_eq!(
            sqlx::query_scalar::<_, i32>("SELECT issued_count FROM admission_pools WHERE id = $1",)
                .bind(admission_pool_id)
                .fetch_one(&pool)
                .await?,
            0
        );
        assert!(
            !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                    SELECT 1 FROM webhook_replay_keys
                    WHERE workspace_id = $1 AND source = 'retention-test' AND event_id = $2
                )",
            )
            .bind(workspace_id)
            .bind(&expired_replay_id)
            .fetch_one(&pool)
            .await?
        );
        assert!(
            !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM fan_sessions WHERE id = $1)",
            )
            .bind(expired_session_id)
            .fetch_one(&pool)
            .await?
        );
        assert!(
            !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM fan_action_tokens WHERE id = $1)",
            )
            .bind(expired_action_id)
            .fetch_one(&pool)
            .await?
        );
        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM fan_action_tokens WHERE id = $1)",
            )
            .bind(recent_consumed_action_id)
            .fetch_one(&pool)
            .await?
        );

        let scrubbed_payload = sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT payload FROM outbox_events WHERE id = $1",
        )
        .bind(scrubbed_event_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            scrubbed_payload
                .get("email")
                .and_then(|value| value.as_str()),
            Some("fan@example.test")
        );
        assert!(scrubbed_payload.get("unsubscribe_token").is_none());
        assert!(scrubbed_payload.get("confirmation_token").is_none());

        let blocked_payload = sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT payload FROM outbox_events WHERE id = $1",
        )
        .bind(blocked_event_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            blocked_payload
                .get("claim_token")
                .and_then(|value| value.as_str()),
            Some("still-needed")
        );
        assert!(
            !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM outbox_events WHERE id = $1)",
            )
            .bind(deleted_event_id)
            .fetch_one(&pool)
            .await?
        );
        assert!(
            !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM webhook_deliveries WHERE id = $1)",
            )
            .bind(deleted_delivery_id)
            .fetch_one(&pool)
            .await?
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM webhook_delivery_attempts WHERE delivery_id = $1",
            )
            .bind(deleted_delivery_id)
            .fetch_one(&pool)
            .await?,
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM audit_events
                    WHERE workspace_id = $1 AND action = 'retention.test'",
            )
            .bind(workspace_id)
            .fetch_one(&pool)
            .await?,
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM fan_consents
                    WHERE workspace_id = $1 AND fan_id = $2",
            )
            .bind(workspace_id)
            .bind(fan_id)
            .fetch_one(&pool)
            .await?,
            1
        );

        let second_stats = worker.run_once().await?;
        assert_eq!(second_stats.expired_admission_passes_reconciled, 0);
        assert_eq!(
            sqlx::query_scalar::<_, i32>("SELECT issued_count FROM admission_pools WHERE id = $1",)
                .bind(admission_pool_id)
                .fetch_one(&pool)
                .await?,
            0,
            "reconciliation must be idempotent"
        );

        pool.close().await;
        Ok(())
    }

    #[test]
    fn stats_report_work_only_for_changed_rows() {
        assert!(!RetentionStats::default().did_work());
        assert!(
            RetentionStats {
                outbox_payloads_scrubbed: 1,
                ..RetentionStats::default()
            }
            .did_work()
        );
    }
}
