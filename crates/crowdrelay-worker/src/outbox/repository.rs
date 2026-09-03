use std::{collections::HashMap, time::Duration};

use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use thiserror::Error;
use uuid::Uuid;

use super::model::{AttemptResolution, DeliveryClaim, OutboxEventClaim};

/// Recipient gate for one delivery, derived from the event type and payload
/// alone, before any database read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EligibilityTarget {
    /// The event type carries no recipient gate.
    NotGated,
    /// The payload named a fan whose current state decides eligibility.
    Fan { fan_id: Uuid, require_consent: bool },
    /// The event type gates on a fan the payload did not name usably.
    MissingRecipient,
}

/// Classifies a delivery's recipient gate.
///
/// Event types outside the gated set are always eligible; a gated type whose
/// payload carries no parseable fan ID is never eligible.
pub(super) fn eligibility_target(event_type: &str, payload: &Value) -> EligibilityTarget {
    let (raw_fan_id, require_consent) = match event_type {
        "event.reminder_due" => (payload.get("fan_id").and_then(Value::as_str), false),
        "event.announcement_due" => (
            payload
                .get("fan")
                .and_then(|fan| fan.get("id"))
                .and_then(Value::as_str),
            true,
        ),
        "crowdrelay.fan_lifecycle.message_requested" => {
            (payload.get("fan_id").and_then(Value::as_str), true)
        }
        _ => return EligibilityTarget::NotGated,
    };

    raw_fan_id
        .and_then(|value| Uuid::parse_str(value).ok())
        .map_or(EligibilityTarget::MissingRecipient, |fan_id| {
            EligibilityTarget::Fan {
                fan_id,
                require_consent,
            }
        })
}

#[derive(Clone, Debug)]
pub(super) struct PgOutboxStore {
    pool: PgPool,
}

impl PgOutboxStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn claim_outbox_events(
        &self,
        worker_id: &str,
        batch_size: i64,
        lease_duration: Duration,
    ) -> Result<Vec<OutboxEventClaim>, StoreError> {
        let mut transaction = self.pool.begin().await.map_err(StoreError::Database)?;

        mark_exhausted_outbox_leases_dead(&mut transaction).await?;

        let claims = sqlx::query_as::<_, OutboxEventClaim>(
            r#"
            WITH candidates AS (
                SELECT event.id
                FROM outbox_events AS event
                WHERE (
                    (
                        event.status = 'pending'
                        AND event.available_at <= now()
                    )
                    OR (
                        event.status = 'processing'
                        AND event.lease_expires_at <= now()
                    )
                )
                AND event.attempts < event.max_attempts
                ORDER BY event.available_at, event.id
                FOR UPDATE OF event SKIP LOCKED
                LIMIT $1
            )
            UPDATE outbox_events AS event
            SET
                status = 'processing',
                attempts = event.attempts + 1,
                locked_at = now(),
                lock_owner = $2,
                lease_expires_at = now() + ($3 * INTERVAL '1 millisecond'),
                last_error_kind = NULL,
                dead_at = NULL
            FROM candidates
            WHERE event.id = candidates.id
            RETURNING
                event.id,
                event.workspace_id,
                event.attempts AS attempt_number
            "#,
        )
        .bind(batch_size)
        .bind(worker_id)
        .bind(duration_millis(lease_duration))
        .fetch_all(&mut *transaction)
        .await
        .map_err(StoreError::Database)?;

        transaction.commit().await.map_err(StoreError::Database)?;
        Ok(claims)
    }

    pub async fn materialize_deliveries_batch(
        &self,
        claims: &[OutboxEventClaim],
        worker_id: &str,
    ) -> Result<u64, StoreError> {
        if claims.is_empty() {
            return Ok(0);
        }
        let event_ids: Vec<Uuid> = claims.iter().map(|claim| claim.id).collect();
        let expected = i64::try_from(event_ids.len()).map_err(|_| StoreError::InvalidValue)?;
        let mut transaction = self.pool.begin().await.map_err(StoreError::Database)?;

        let (inserted, completed) = sqlx::query_as::<_, (i64, i64)>(
            r#"
            WITH eligible AS (
                SELECT event.id, event.workspace_id
                FROM outbox_events AS event
                WHERE event.id = ANY($1)
                  AND event.status = 'processing'
                  AND event.lock_owner = $2
            ), inserted AS (
                INSERT INTO webhook_deliveries (
                    workspace_id,
                    outbox_event_id,
                    endpoint_id,
                    max_attempts
                )
                SELECT
                    endpoint.workspace_id,
                    event.id,
                    endpoint.id,
                    endpoint.max_attempts
                FROM eligible AS event
                JOIN webhook_endpoints AS endpoint
                  ON endpoint.workspace_id = event.workspace_id
                 AND endpoint.active
                ON CONFLICT (workspace_id, outbox_event_id, endpoint_id)
                DO NOTHING
                RETURNING 1
            ), completed AS (
                UPDATE outbox_events AS event
                SET
                    status = 'delivered',
                    locked_at = NULL,
                    lock_owner = NULL,
                    lease_expires_at = NULL,
                    last_error_kind = NULL,
                    delivered_at = now(),
                    dead_at = NULL
                FROM eligible
                WHERE event.id = eligible.id
                  AND event.workspace_id = eligible.workspace_id
                  AND event.status = 'processing'
                  AND event.lock_owner = $2
                RETURNING 1
            )
            SELECT
                (SELECT count(*)::bigint FROM inserted),
                (SELECT count(*)::bigint FROM completed)
            "#,
        )
        .bind(&event_ids)
        .bind(worker_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(StoreError::Database)?;

        if completed != expected {
            return Err(StoreError::LostLease);
        }
        transaction.commit().await.map_err(StoreError::Database)?;
        u64::try_from(inserted).map_err(|_| StoreError::InvalidValue)
    }

    pub async fn fail_outbox_event(
        &self,
        claim: &OutboxEventClaim,
        worker_id: &str,
        retryable: bool,
        retry_delay: Duration,
        error_kind: &'static str,
    ) -> Result<(), StoreError> {
        let updated = sqlx::query(
            r#"
            UPDATE outbox_events
            SET
                status = CASE
                    WHEN $4 AND attempts < max_attempts THEN 'pending'
                    ELSE 'dead'
                END,
                available_at = CASE
                    WHEN $4 AND attempts < max_attempts
                        THEN now() + ($5 * INTERVAL '1 millisecond')
                    ELSE available_at
                END,
                locked_at = NULL,
                lock_owner = NULL,
                lease_expires_at = NULL,
                last_error_kind = $6,
                delivered_at = NULL,
                dead_at = CASE
                    WHEN $4 AND attempts < max_attempts THEN NULL
                    ELSE now()
                END
            WHERE workspace_id = $1
              AND id = $2
              AND status = 'processing'
              AND lock_owner = $3
            "#,
        )
        .bind(claim.workspace_id)
        .bind(claim.id)
        .bind(worker_id)
        .bind(retryable)
        .bind(duration_millis(retry_delay))
        .bind(error_kind)
        .execute(&self.pool)
        .await
        .map_err(StoreError::Database)?
        .rows_affected();

        exactly_one(updated)
    }

    pub async fn claim_deliveries(
        &self,
        worker_id: &str,
        batch_size: i64,
        lease_duration: Duration,
    ) -> Result<Vec<DeliveryClaim>, StoreError> {
        let mut transaction = self.pool.begin().await.map_err(StoreError::Database)?;

        record_expired_delivery_attempts(&mut transaction).await?;
        cancel_inactive_endpoint_deliveries(&mut transaction).await?;
        mark_exhausted_delivery_leases_dead(&mut transaction).await?;

        let claims = sqlx::query_as::<_, DeliveryClaim>(
            r#"
            WITH candidates AS (
                SELECT delivery.id
                FROM webhook_deliveries AS delivery
                JOIN webhook_endpoints AS endpoint
                    ON endpoint.workspace_id = delivery.workspace_id
                    AND endpoint.id = delivery.endpoint_id
                    AND endpoint.active
                WHERE (
                    (
                        delivery.status = 'pending'
                        AND delivery.available_at <= now()
                    )
                    OR (
                        delivery.status = 'processing'
                        AND delivery.lease_expires_at <= now()
                    )
                )
                AND delivery.attempt_count < delivery.max_attempts
                ORDER BY delivery.available_at, delivery.id
                FOR UPDATE OF delivery SKIP LOCKED
                LIMIT $1
            ),
            claimed AS (
                UPDATE webhook_deliveries AS delivery
                SET
                    status = 'processing',
                    attempt_count = delivery.attempt_count + 1,
                    locked_at = now(),
                    lock_owner = $2,
                    lease_expires_at = now() + ($3 * INTERVAL '1 millisecond'),
                    last_response_status = NULL,
                    last_error_kind = NULL,
                    dead_at = NULL,
                    cancelled_at = NULL
                FROM candidates
                WHERE delivery.id = candidates.id
                RETURNING delivery.*
            )
            SELECT
                claimed.id AS delivery_id,
                claimed.workspace_id,
                event.id AS event_id,
                event.event_type,
                event.event_version,
                event.payload,
                event.created_at AS event_created_at,
                event.request_id,
                event.trace_id,
                event.action_id,
                endpoint.id AS endpoint_id,
                endpoint.url AS endpoint_url,
                endpoint.signing_secret_ref,
                endpoint.timeout_ms,
                claimed.attempt_count AS attempt_number,
                claimed.max_attempts
            FROM claimed
            JOIN outbox_events AS event
                ON event.workspace_id = claimed.workspace_id
                AND event.id = claimed.outbox_event_id
            JOIN webhook_endpoints AS endpoint
                ON endpoint.workspace_id = claimed.workspace_id
                AND endpoint.id = claimed.endpoint_id
            ORDER BY claimed.available_at, claimed.id
            "#,
        )
        .bind(batch_size)
        .bind(worker_id)
        .bind(duration_millis(lease_duration))
        .fetch_all(&mut *transaction)
        .await
        .map_err(StoreError::Database)?;

        transaction.commit().await.map_err(StoreError::Database)?;
        Ok(claims)
    }

    /// Resolves recipient eligibility for a whole claimed batch in one query.
    ///
    /// Returns one entry per active fan among `recipients`, carrying whether
    /// that fan's latest marketing consent is granted. A missing entry means
    /// the fan is not active and is therefore never eligible.
    pub async fn active_fan_marketing_consent(
        &self,
        recipients: &[(Uuid, Uuid)],
    ) -> Result<HashMap<(Uuid, Uuid), bool>, StoreError> {
        if recipients.is_empty() {
            return Ok(HashMap::new());
        }
        let (workspace_ids, fan_ids): (Vec<Uuid>, Vec<Uuid>) = recipients.iter().copied().unzip();

        let rows = sqlx::query_as::<_, (Uuid, Uuid, bool)>(
            r#"
            SELECT
                fan.workspace_id,
                fan.id,
                COALESCE((
                    SELECT consent.granted
                    FROM fan_consents AS consent
                    WHERE consent.workspace_id = fan.workspace_id
                      AND consent.fan_id = fan.id
                      AND consent.purpose = 'marketing'
                    ORDER BY consent.recorded_at DESC, consent.id DESC
                    LIMIT 1
                ), false)
            FROM fans AS fan
            JOIN (
                SELECT *
                FROM unnest($1::uuid[], $2::uuid[])
                    AS pair(workspace_id, fan_id)
            ) AS requested
                ON requested.workspace_id = fan.workspace_id
                AND requested.fan_id = fan.id
            WHERE fan.status = 'active'
            "#,
        )
        .bind(&workspace_ids)
        .bind(&fan_ids)
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::Database)?;

        Ok(rows
            .into_iter()
            .map(|(workspace_id, fan_id, granted)| ((workspace_id, fan_id), granted))
            .collect())
    }

    pub async fn finish_delivery(
        &self,
        claim: &DeliveryClaim,
        worker_id: &str,
        resolution: &AttemptResolution,
    ) -> Result<(), StoreError> {
        let mut transaction = self.pool.begin().await.map_err(StoreError::Database)?;

        let updated = sqlx::query(
            r#"
            UPDATE webhook_deliveries
            SET
                status = $5,
                available_at = CASE
                    WHEN $6 = 'retry'
                        THEN now() + ($7 * INTERVAL '1 millisecond')
                    ELSE available_at
                END,
                locked_at = NULL,
                lock_owner = NULL,
                lease_expires_at = NULL,
                last_response_status = $8,
                last_error_kind = $9,
                delivered_at = CASE WHEN $6 = 'delivered' THEN now() ELSE NULL END,
                dead_at = CASE WHEN $6 = 'dead' THEN now() ELSE NULL END,
                cancelled_at = NULL
            WHERE workspace_id = $1
              AND id = $2
              AND status = 'processing'
              AND lock_owner = $3
              AND attempt_count = $4
            "#,
        )
        .bind(claim.workspace_id)
        .bind(claim.delivery_id)
        .bind(worker_id)
        .bind(claim.attempt_number)
        .bind(resolution.outcome.delivery_status())
        .bind(resolution.outcome.as_str())
        .bind(resolution.retry_delay_ms)
        .bind(resolution.response_status)
        .bind(resolution.error_kind)
        .execute(&mut *transaction)
        .await
        .map_err(StoreError::Database)?
        .rows_affected();

        if updated != 1 {
            return Err(StoreError::LostLease);
        }

        sqlx::query(
            r#"
            INSERT INTO webhook_delivery_attempts (
                workspace_id,
                delivery_id,
                attempt_number,
                started_at,
                finished_at,
                outcome,
                response_status,
                error_kind,
                duration_ms
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            "#,
        )
        .bind(claim.workspace_id)
        .bind(claim.delivery_id)
        .bind(claim.attempt_number)
        .bind(resolution.started_at)
        .bind(resolution.finished_at)
        .bind(resolution.outcome.as_str())
        .bind(resolution.response_status)
        .bind(resolution.error_kind)
        .bind(resolution.duration_ms)
        .execute(&mut *transaction)
        .await
        .map_err(StoreError::Database)?;

        // When the outcome is ambiguous (transport exhausted after
        // timeouts where the request may have reached the provider),
        // transition the linked autopilot action to `unknown` in the
        // same transaction. This prevents a false SUCCEEDED/FAILED and
        // enters the action into the reconciliation queue. The action
        // ledger trigger (migration 0190) maps `unknown → UNKNOWN`.
        //
        // The guard is exactly the two statuses the ledger permits
        // `UNKNOWN` from — `RUNNING` and `SUCCEEDED`. It used to read
        // `('succeeded', 'processing', 'queued', 'running')`, and both extra
        // values were wrong in a different way. `running` is not a value the
        // `viryaos_autopilot_actions` status CHECK allows at all, so that arm
        // never matched anything. `queued` matched, and matching was the
        // damage: `QUEUED -> UNKNOWN` is not a legal ledger transition, so the
        // trigger raised `check_violation` and took this whole transaction
        // down with it — the attempt row and the lease update along with it.
        //
        // Nothing is lost by narrowing it. `emit_external_action` only runs
        // after the action is claimed into `processing`, so the outbox event
        // hanging off a `queued` action is the `approval_requested`
        // notification, not an execution dispatch. A notification whose
        // delivery went ambiguous says nothing about an external side effect,
        // because none was attempted.
        if resolution.outcome == super::model::AttemptOutcome::Ambiguous
            && let Some(action_id) = claim.action_id
        {
            sqlx::query(
                r#"
                UPDATE viryaos_autopilot_actions
                SET status = 'unknown',
                    updated_at = now()
                WHERE id = $1
                  AND workspace_id = $2
                  AND status IN ('succeeded', 'processing')
                "#,
            )
            .bind(action_id)
            .bind(claim.workspace_id)
            .execute(&mut *transaction)
            .await
            .map_err(StoreError::Database)?;
        }

        transaction.commit().await.map_err(StoreError::Database)?;
        Ok(())
    }
}

async fn mark_exhausted_outbox_leases_dead(
    transaction: &mut Transaction<'_, Postgres>,
) -> Result<(), StoreError> {
    sqlx::query(
        r#"
        UPDATE outbox_events
        SET
            status = 'dead',
            locked_at = NULL,
            lock_owner = NULL,
            lease_expires_at = NULL,
            last_error_kind = 'lease_expired_after_max_attempts',
            delivered_at = NULL,
            dead_at = now()
        WHERE status = 'processing'
          AND lease_expires_at <= now()
          AND attempts >= max_attempts
        "#,
    )
    .execute(&mut **transaction)
    .await
    .map_err(StoreError::Database)?;
    Ok(())
}

async fn mark_exhausted_delivery_leases_dead(
    transaction: &mut Transaction<'_, Postgres>,
) -> Result<(), StoreError> {
    // Mark exhausted delivery leases as dead. A lease expiring after max
    // attempts means the worker crashed during the final attempt — we
    // don't know if the request reached the provider. This is externally
    // ambiguous, so also transition any linked autopilot action to
    // `unknown` in the same transaction.
    let dead_delivery_ids: Vec<Uuid> = sqlx::query_scalar(
        r#"
        UPDATE webhook_deliveries
        SET
            status = 'dead',
            locked_at = NULL,
            lock_owner = NULL,
            lease_expires_at = NULL,
            last_error_kind = 'lease_expired_after_max_attempts',
            delivered_at = NULL,
            dead_at = now(),
            cancelled_at = NULL
        WHERE status = 'processing'
          AND lease_expires_at <= now()
          AND attempt_count >= max_attempts
        RETURNING id
        "#,
    )
    .fetch_all(&mut **transaction)
    .await
    .map_err(StoreError::Database)?;

    if !dead_delivery_ids.is_empty() {
        // Transition linked autopilot actions to unknown — the worker
        // crashed during the final attempt and we don't know if the
        // provider received the request.
        //
        // Same guard as `resolve_delivery_attempt`, for the same reasons, and
        // it mattered more here: this is a sweep. One `queued` action in the
        // batch raised the ledger trigger's `check_violation`, which rolled
        // back the `webhook_deliveries` dead-lettering above it, so the same
        // rows were re-selected on the next pass and dead-lettering stalled
        // for every delivery — not just the one that poisoned it.
        sqlx::query(
            r#"
            UPDATE viryaos_autopilot_actions AS action
            SET status = 'unknown', updated_at = now()
            FROM webhook_deliveries AS delivery
            JOIN outbox_events AS event
                ON event.workspace_id = delivery.workspace_id
                AND event.id = delivery.outbox_event_id
            WHERE delivery.id = ANY($1)
              AND event.action_id = action.id
              AND action.status IN ('succeeded', 'processing')
            "#,
        )
        .bind(&dead_delivery_ids)
        .execute(&mut **transaction)
        .await
        .map_err(StoreError::Database)?;
    }

    Ok(())
}

async fn record_expired_delivery_attempts(
    transaction: &mut Transaction<'_, Postgres>,
) -> Result<(), StoreError> {
    sqlx::query(
        r#"
        INSERT INTO webhook_delivery_attempts (
            workspace_id,
            delivery_id,
            attempt_number,
            started_at,
            finished_at,
            outcome,
            response_status,
            error_kind,
            duration_ms
        )
        SELECT
            delivery.workspace_id,
            delivery.id,
            delivery.attempt_count,
            delivery.locked_at,
            delivery.lease_expires_at,
            CASE
                WHEN NOT endpoint.active THEN 'cancelled'
                WHEN delivery.attempt_count >= delivery.max_attempts THEN 'dead'
                ELSE 'retry'
            END,
            NULL,
            CASE
                WHEN NOT endpoint.active THEN 'endpoint_inactive'
                ELSE 'lease_expired'
            END,
            LEAST(
                GREATEST(
                    EXTRACT(EPOCH FROM (
                        delivery.lease_expires_at - delivery.locked_at
                    )) * 1000,
                    0
                ),
                2147483647
            )::integer
        FROM webhook_deliveries AS delivery
        JOIN webhook_endpoints AS endpoint
            ON endpoint.workspace_id = delivery.workspace_id
            AND endpoint.id = delivery.endpoint_id
        WHERE delivery.status = 'processing'
          AND delivery.lease_expires_at <= now()
        ON CONFLICT (workspace_id, delivery_id, attempt_number)
        DO NOTHING
        "#,
    )
    .execute(&mut **transaction)
    .await
    .map_err(StoreError::Database)?;
    Ok(())
}

async fn cancel_inactive_endpoint_deliveries(
    transaction: &mut Transaction<'_, Postgres>,
) -> Result<(), StoreError> {
    sqlx::query(
        r#"
        UPDATE webhook_deliveries AS delivery
        SET
            status = 'cancelled',
            locked_at = NULL,
            lock_owner = NULL,
            lease_expires_at = NULL,
            last_error_kind = 'endpoint_inactive',
            delivered_at = NULL,
            dead_at = NULL,
            cancelled_at = now()
        FROM webhook_endpoints AS endpoint
        WHERE endpoint.workspace_id = delivery.workspace_id
          AND endpoint.id = delivery.endpoint_id
          AND NOT endpoint.active
          AND (
              delivery.status = 'pending'
              OR (
                  delivery.status = 'processing'
                  AND delivery.lease_expires_at <= now()
              )
          )
        "#,
    )
    .execute(&mut **transaction)
    .await
    .map_err(StoreError::Database)?;
    Ok(())
}

fn duration_millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn exactly_one(rows_affected: u64) -> Result<(), StoreError> {
    if rows_affected == 1 {
        Ok(())
    } else {
        Err(StoreError::LostLease)
    }
}

#[derive(Debug, Error)]
pub(super) enum StoreError {
    #[error("PostgreSQL outbox operation failed")]
    Database(#[source] sqlx::Error),

    #[error("outbox lease is no longer owned by this worker")]
    LostLease,

    #[error("outbox value could not be represented safely")]
    InvalidValue,
}

impl StoreError {
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Database(_) => "database",
            Self::LostLease => "lost_lease",
            Self::InvalidValue => "invalid_value",
        }
    }
}

#[cfg(test)]
mod eligibility_tests {
    use super::*;

    #[test]
    fn ungated_event_types_need_no_recipient_lookup() {
        assert_eq!(
            eligibility_target("fan.created", &serde_json::json!({"fan_id": "not-a-uuid"})),
            EligibilityTarget::NotGated
        );
    }

    #[test]
    fn reminders_gate_on_the_fan_without_consent() {
        let fan_id = Uuid::now_v7();
        assert_eq!(
            eligibility_target(
                "event.reminder_due",
                &serde_json::json!({"fan_id": fan_id.to_string()})
            ),
            EligibilityTarget::Fan {
                fan_id,
                require_consent: false
            }
        );
    }

    #[test]
    fn marketing_event_types_gate_on_consent() {
        let fan_id = Uuid::now_v7();
        assert_eq!(
            eligibility_target(
                "event.announcement_due",
                &serde_json::json!({"fan": {"id": fan_id.to_string()}})
            ),
            EligibilityTarget::Fan {
                fan_id,
                require_consent: true
            }
        );
        assert_eq!(
            eligibility_target(
                "crowdrelay.fan_lifecycle.message_requested",
                &serde_json::json!({"fan_id": fan_id.to_string()})
            ),
            EligibilityTarget::Fan {
                fan_id,
                require_consent: true
            }
        );
    }

    #[test]
    fn gated_event_types_reject_malformed_or_missing_recipients() {
        assert_eq!(
            eligibility_target(
                "event.reminder_due",
                &serde_json::json!({"fan_id": "malformed"})
            ),
            EligibilityTarget::MissingRecipient
        );
        assert_eq!(
            eligibility_target("event.reminder_due", &serde_json::json!({})),
            EligibilityTarget::MissingRecipient
        );
        assert_eq!(
            eligibility_target(
                "event.announcement_due",
                &serde_json::json!({"fan": {"id": 42}})
            ),
            EligibilityTarget::MissingRecipient
        );
    }
}

#[cfg(test)]
mod postgres_tests {
    use std::time::Duration;

    use sqlx::postgres::PgPoolOptions;
    use time::OffsetDateTime;
    use uuid::Uuid;

    use super::*;
    use crate::outbox::model::AttemptOutcome;

    /// Exercises real PostgreSQL lease recovery and delivery materialization.
    ///
    /// Run explicitly against a disposable database:
    ///
    /// `CROWDRELAY_OUTBOX_TEST_DATABASE_URL=postgres://... cargo test \
    /// -p crowdrelay-worker postgres_outbox_round_trip -- --ignored`
    #[tokio::test]
    #[ignore = "requires CROWDRELAY_OUTBOX_TEST_DATABASE_URL and a disposable PostgreSQL database"]
    async fn postgres_outbox_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let database_url = std::env::var("CROWDRELAY_OUTBOX_TEST_DATABASE_URL").map_err(|e| {
            format!("CROWDRELAY_OUTBOX_TEST_DATABASE_URL must target a disposable database: {e}")
        })?;
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await?;
        crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

        let workspace_id = Uuid::now_v7();
        let endpoint_id = Uuid::now_v7();
        let inactive_endpoint_id = Uuid::now_v7();
        let event_id = Uuid::now_v7();
        let fan_id = Uuid::now_v7();
        sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Outbox Test')")
            .bind(workspace_id)
            .bind(format!("outbox-{workspace_id}"))
            .execute(&pool)
            .await?;
        sqlx::query(
            "INSERT INTO fans (id, workspace_id, normalized_email, status) \
             VALUES ($1, $2, $3, 'active')",
        )
        .bind(fan_id)
        .bind(workspace_id)
        .bind(format!("{fan_id}@example.test"))
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO webhook_endpoints (
                id, workspace_id, name, url, signing_secret_ref, active
            )
            VALUES
                ($1, $3, 'active', 'https://n8n.example/webhook', 'test/current', true),
                ($2, $3, 'inactive', 'https://n8n.example/disabled', 'test/current', false)
            "#,
        )
        .bind(endpoint_id)
        .bind(inactive_endpoint_id)
        .bind(workspace_id)
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO outbox_events (
                id, workspace_id, event_type, event_version, payload, request_id,
                available_at
            )
            VALUES (
                $1,
                $2,
                'fan.created',
                1,
                '{"fan_id":"redacted"}',
                'request-test',
                '-infinity'
            )
            "#,
        )
        .bind(event_id)
        .bind(workspace_id)
        .execute(&pool)
        .await?;

        let store = PgOutboxStore::new(pool.clone());
        // An active fan with no recorded consent is eligible for event types
        // that do not gate on consent, and never for the ones that do.
        assert_eq!(
            store
                .active_fan_marketing_consent(&[(workspace_id, fan_id)])
                .await?
                .get(&(workspace_id, fan_id)),
            Some(&false)
        );
        sqlx::query(
            "INSERT INTO fan_consents (workspace_id, fan_id, purpose, granted, policy_version, source) \
             VALUES ($1, $2, 'marketing', true, 'test-v1', 'integration-test')",
        )
        .bind(workspace_id)
        .bind(fan_id)
        .execute(&pool)
        .await?;
        assert_eq!(
            store
                .active_fan_marketing_consent(&[(workspace_id, fan_id)])
                .await?
                .get(&(workspace_id, fan_id)),
            Some(&true)
        );
        sqlx::query(
            "INSERT INTO fan_consents (workspace_id, fan_id, purpose, granted, policy_version, source) \
             VALUES ($1, $2, 'marketing', false, 'test-v1', 'integration-test')",
        )
        .bind(workspace_id)
        .bind(fan_id)
        .execute(&pool)
        .await?;
        assert_eq!(
            store
                .active_fan_marketing_consent(&[(workspace_id, fan_id)])
                .await?
                .get(&(workspace_id, fan_id)),
            Some(&false)
        );
        sqlx::query("UPDATE fans SET status = 'unsubscribed' WHERE id = $1")
            .bind(fan_id)
            .execute(&pool)
            .await?;
        // A fan that is no longer active drops out of the batch entirely, so
        // every gated event type treats it as ineligible.
        assert!(
            store
                .active_fan_marketing_consent(&[(workspace_id, fan_id)])
                .await?
                .is_empty()
        );
        let first_claim = store
            .claim_outbox_events("worker-a", 1, Duration::from_secs(90))
            .await?
            .pop()
            .ok_or("event must be claimed")?;
        assert_eq!(first_claim.id, event_id);
        assert_eq!(first_claim.workspace_id, workspace_id);
        assert_eq!(first_claim.attempt_number, 1);

        expire_outbox_lease(&pool, event_id).await?;
        let recovered_claim = store
            .claim_outbox_events("worker-b", 1, Duration::from_secs(90))
            .await?
            .pop()
            .ok_or("expired event must be recovered")?;
        assert_eq!(recovered_claim.attempt_number, 2);
        assert_eq!(
            store
                .materialize_deliveries_batch(std::slice::from_ref(&recovered_claim), "worker-b")
                .await?,
            1
        );

        let first_delivery = store
            .claim_deliveries("worker-a", 1, Duration::from_secs(90))
            .await?
            .pop()
            .ok_or("delivery must be claimed")?;
        assert_eq!(first_delivery.endpoint_id, endpoint_id);
        assert_eq!(first_delivery.attempt_number, 1);

        expire_delivery_lease(&pool, first_delivery.delivery_id).await?;
        let recovered_delivery = store
            .claim_deliveries("worker-b", 1, Duration::from_secs(90))
            .await?
            .pop()
            .ok_or("expired delivery must be recovered")?;
        assert_eq!(recovered_delivery.attempt_number, 2);

        let now = OffsetDateTime::now_utc();
        store
            .finish_delivery(
                &recovered_delivery,
                "worker-b",
                &AttemptResolution {
                    outcome: AttemptOutcome::Delivered,
                    response_status: Some(204),
                    error_kind: None,
                    retry_delay_ms: 0,
                    started_at: now,
                    finished_at: now,
                    duration_ms: 7,
                },
            )
            .await?;

        let delivery_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM webhook_deliveries WHERE id = $1")
                .bind(recovered_delivery.delivery_id)
                .fetch_one(&pool)
                .await?;
        let attempts = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM webhook_delivery_attempts WHERE delivery_id = $1",
        )
        .bind(recovered_delivery.delivery_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(delivery_status, "delivered");
        assert_eq!(attempts, 2);

        // The test database is disposable. Consent rows are append-only and intentionally
        // prevent deleting their workspace during teardown.
        pool.close().await;
        Ok(())
    }

    async fn expire_outbox_lease(
        pool: &PgPool,
        event_id: Uuid,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let result = sqlx::query(
            r#"
            UPDATE outbox_events
            SET
                locked_at = now() - INTERVAL '2 minutes',
                lease_expires_at = now() - INTERVAL '1 minute'
            WHERE id = $1
              AND status = 'processing'
              AND lock_owner IS NOT NULL
            "#,
        )
        .bind(event_id)
        .execute(pool)
        .await?;

        assert_eq!(
            result.rows_affected(),
            1,
            "expected one owned outbox lease to expire"
        );
        Ok(())
    }

    async fn expire_delivery_lease(
        pool: &PgPool,
        delivery_id: Uuid,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let result = sqlx::query(
            r#"
            UPDATE webhook_deliveries
            SET
                locked_at = now() - INTERVAL '2 minutes',
                lease_expires_at = now() - INTERVAL '1 minute'
            WHERE id = $1
              AND status = 'processing'
              AND lock_owner IS NOT NULL
            "#,
        )
        .bind(delivery_id)
        .execute(pool)
        .await?;

        assert_eq!(
            result.rows_affected(),
            1,
            "expected one owned delivery lease to expire"
        );
        Ok(())
    }
}
