use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::{FromRow, PgPool};
use time::{Duration as TimeDuration, OffsetDateTime};
use uuid::Uuid;

const MAX_ATTEMPTS: i32 = 6;
const ACK_TTL: TimeDuration = TimeDuration::minutes(15);

#[derive(Debug, Clone, FromRow)]
pub struct ClaimedDelivery {
    pub id: Uuid,
    pub endpoint_id: Uuid,
    pub title: String,
    pub body: String,
    pub target_path: String,
    pub collapse_key: Option<String>,
    pub transport: String,
    pub endpoint_address: String,
    pub p256dh: Option<String>,
    pub auth_secret: Option<String>,
    pub claim_token: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderTerminal {
    Failed,
    Ambiguous,
}

#[derive(Clone, Debug)]
pub struct PushDeliveryRepository {
    database: PgPool,
    workspace_id: Uuid,
    operation_timeout: Duration,
    quiet_timezone: String,
}

impl PushDeliveryRepository {
    pub fn new(
        database: PgPool,
        workspace_id: Uuid,
        operation_timeout: Duration,
        quiet_timezone: String,
    ) -> Self {
        Self {
            database,
            workspace_id,
            operation_timeout,
            quiet_timezone,
        }
    }

    pub async fn feature_enabled(&self) -> Result<bool> {
        self.with_timeout(
            sqlx::query_scalar::<_, bool>(
                r#"
                SELECT COALESCE((
                    SELECT enabled
                    FROM ecosystem_feature_flags
                    WHERE workspace_id = $1 AND key = 'push_delivery_enabled'
                ), false)
                "#,
            )
            .bind(self.workspace_id)
            .fetch_one(&self.database),
            "push delivery feature flag lookup",
        )
        .await
    }

    pub async fn maintain(&self) -> Result<()> {
        let stale_claim_seconds = seconds_i64(self.operation_timeout.saturating_mul(3));
        let mut transaction = self
            .database
            .begin()
            .await
            .context("push maintenance transaction")?;

        sqlx::query(
            r#"
            UPDATE fan_push_endpoints endpoint
            SET active = false,
                invalidated_at = COALESCE(endpoint.invalidated_at, now()),
                last_error_code = 'staff_session_expired',
                updated_at = now()
            WHERE endpoint.workspace_id = $1
              AND endpoint.audience_kind = 'staff'
              AND endpoint.active
              AND EXISTS (
                  SELECT 1
                  FROM staff_device_sessions session
                  WHERE session.workspace_id = endpoint.workspace_id
                    AND session.token_hash = endpoint.principal_hash
                    AND (session.revoked_at IS NOT NULL OR session.expires_at <= now())
              )
            "#,
        )
        .bind(self.workspace_id)
        .execute(&mut *transaction)
        .await
        .context("invalidate expired staff-session push endpoints")?;

        sqlx::query(
            r#"
            UPDATE fan_push_deliveries delivery
            -- The cause decides the code, and the two causes are different
            -- facts about different things.
            --
            -- This used to report every sweep as `fan_or_consent_ineligible`,
            -- including the case where the *endpoint* had gone away. A fan who
            -- reinstalled the app eight times left seven dead registrations,
            -- and the twenty-one deliveries queued against them were all
            -- reported as though that person had withdrawn consent. It read as
            -- a consent violation — messaging people who had said no — when
            -- the fan was consenting throughout and two messages reached them
            -- normally.
            --
            -- `endpoint_inactive` also says something operationally different:
            -- there is nothing to fix and nothing to retry. A withdrawn
            -- consent is a person's decision; a dead endpoint is a phone that
            -- reinstalled.
            SET status = 'failed', error_code = CASE
                    WHEN NOT EXISTS (
                        SELECT 1 FROM fan_push_endpoints endpoint
                        WHERE endpoint.workspace_id = delivery.workspace_id
                          AND endpoint.id = delivery.endpoint_id
                          AND endpoint.audience_kind = delivery.audience_kind
                          AND endpoint.fan_id IS NOT DISTINCT FROM delivery.fan_id
                          AND endpoint.active AND endpoint.invalidated_at IS NULL
                    ) THEN 'endpoint_inactive'
                    WHEN delivery.audience_kind = 'fan' THEN 'fan_or_consent_ineligible'
                    WHEN delivery.audience_kind = 'beacon' THEN 'beacon_session_ineligible'
                    ELSE 'staff_endpoint_ineligible'
                END,
                completed_at = now(), updated_at = now()
            WHERE delivery.workspace_id = $1
              AND delivery.status IN ('queued','retry_wait','claimed')
              AND (
                NOT EXISTS (
                    SELECT 1 FROM fan_push_endpoints endpoint
                    WHERE endpoint.workspace_id = delivery.workspace_id
                      AND endpoint.id = delivery.endpoint_id
                      AND endpoint.audience_kind = delivery.audience_kind
                      AND endpoint.fan_id IS NOT DISTINCT FROM delivery.fan_id
                      AND endpoint.active AND endpoint.invalidated_at IS NULL
                )
                OR (
                    delivery.audience_kind = 'fan'
                    AND (
                        NOT EXISTS (
                            SELECT 1 FROM fans fan
                            WHERE fan.workspace_id = delivery.workspace_id
                              AND fan.id = delivery.fan_id AND fan.status = 'active'
                        )
                        OR COALESCE((
                            SELECT consent.granted
                            FROM fan_consents consent
                            WHERE consent.workspace_id = delivery.workspace_id
                              AND consent.fan_id = delivery.fan_id
                              AND consent.purpose = 'marketing'
                            ORDER BY consent.recorded_at DESC, consent.id DESC
                            LIMIT 1
                        ), false) = false
                    )
                )
                OR (
                    delivery.audience_kind = 'beacon'
                    AND NOT EXISTS (
                        SELECT 1
                        FROM fan_push_endpoints beacon_endpoint
                        JOIN viryaos_beacon_signal_sessions session
                          ON session.workspace_id = beacon_endpoint.workspace_id
                         AND session.token_hash = beacon_endpoint.principal_hash
                         AND session.revoked_at IS NULL
                         AND session.expires_at > now()
                        JOIN viryaos_beacon_signal_profiles profile
                          ON profile.workspace_id = session.workspace_id
                         AND profile.beacon_id = session.beacon_id
                         AND profile.status = 'active'
                        JOIN viryaos_beacons beacon
                          ON beacon.workspace_id = session.workspace_id
                         AND beacon.id = session.beacon_id
                         AND beacon.active
                         AND beacon.verified
                         AND beacon.accepts_outreach
                         AND NOT beacon.do_not_contact
                        WHERE beacon_endpoint.workspace_id = delivery.workspace_id
                          AND beacon_endpoint.id = delivery.endpoint_id
                          AND beacon_endpoint.audience_kind = 'beacon'
                    )
                )
              )
            "#,
        )
        .bind(self.workspace_id)
        .execute(&mut *transaction)
        .await
        .context("invalidate ineligible push deliveries")?;

        // Category opt-out is terminal for already-materialized pushes. Leaving
        // them queued would allow stale notifications to fire if the fan later
        // re-enables a category. Quiet hours are intentionally *not* terminal;
        // claim_due defers those until the quiet window ends.
        sqlx::query(
            r#"
            UPDATE fan_push_deliveries delivery
            SET status = 'failed',
                error_code = 'preference_disabled',
                completed_at = now(),
                updated_at = now()
            FROM fan_push_preferences preference
            WHERE delivery.workspace_id = $1
              AND delivery.workspace_id = preference.workspace_id
              AND delivery.fan_id = preference.fan_id
              AND delivery.audience_kind = 'fan'
              AND delivery.status IN ('queued','retry_wait')
              AND delivery.category <> 'essential'
              AND CASE delivery.category
                  WHEN 'shows' THEN NOT preference.shows_enabled
                  WHEN 'releases' THEN NOT preference.releases_enabled
                  WHEN 'community' THEN NOT preference.community_enabled
                  WHEN 'merch' THEN NOT preference.merch_enabled
                  ELSE false
              END
            "#,
        )
        .bind(self.workspace_id)
        .execute(&mut *transaction)
        .await
        .context("suppress push deliveries disabled by fan preference")?;

        sqlx::query(
            r#"
            UPDATE fan_push_deliveries
            SET status = 'queued', claim_token = NULL, claimed_at = NULL,
                available_at = now(), error_code = 'claim_recovered_before_provider',
                updated_at = now()
            WHERE workspace_id = $1 AND status = 'claimed'
              AND provider_started_at IS NULL
              AND claimed_at < now() - ($2::bigint * interval '1 second')
            "#,
        )
        .bind(self.workspace_id)
        .bind(stale_claim_seconds)
        .execute(&mut *transaction)
        .await
        .context("recover stale pre-provider push claims")?;

        sqlx::query(
            r#"
            UPDATE fan_push_deliveries
            SET status = 'ambiguous', error_code = 'worker_lost_after_provider_start',
                completed_at = now(), updated_at = now()
            WHERE workspace_id = $1 AND status = 'provider_started'
              AND provider_started_at < now() - ($2::bigint * interval '1 second')
            "#,
        )
        .bind(self.workspace_id)
        .bind(stale_claim_seconds)
        .execute(&mut *transaction)
        .await
        .context("fail closed stale provider-started push deliveries")?;

        sqlx::query(
            r#"
            UPDATE fan_push_deliveries
            SET status = 'failed', error_code = 'device_ack_timeout',
                completed_at = now(), updated_at = now()
            WHERE workspace_id = $1 AND status = 'provider_accepted'
              AND ack_deadline IS NOT NULL AND ack_deadline <= now()
            "#,
        )
        .bind(self.workspace_id)
        .execute(&mut *transaction)
        .await
        .context("expire unacknowledged push deliveries")?;

        transaction
            .commit()
            .await
            .context("commit push maintenance")?;
        self.reconcile_campaigns().await
    }

    pub async fn claim_due(&self, limit: i64) -> Result<Vec<ClaimedDelivery>> {
        self.with_timeout(
            sqlx::query_as::<_, ClaimedDelivery>(
                r#"
                WITH candidate AS (
                    SELECT delivery.id
                    FROM fan_push_deliveries delivery
                    JOIN fan_push_endpoints endpoint
                      ON endpoint.workspace_id = delivery.workspace_id
                     AND endpoint.id = delivery.endpoint_id
                     AND endpoint.audience_kind = delivery.audience_kind
                     AND endpoint.fan_id IS NOT DISTINCT FROM delivery.fan_id
                    LEFT JOIN fans fan
                      ON delivery.audience_kind = 'fan'
                     AND fan.workspace_id = delivery.workspace_id
                     AND fan.id = delivery.fan_id
                    LEFT JOIN fan_push_preferences preference
                      ON delivery.audience_kind = 'fan'
                     AND preference.workspace_id = delivery.workspace_id
                     AND preference.fan_id = delivery.fan_id
                    WHERE delivery.workspace_id = $1
                      AND delivery.status IN ('queued','retry_wait')
                      AND delivery.available_at <= now()
                      AND delivery.attempt_count < $3
                      AND endpoint.active AND endpoint.invalidated_at IS NULL
                      AND (
                          delivery.audience_kind <> 'fan'
                          OR delivery.category = 'essential'
                          OR (
                              CASE delivery.category
                                  WHEN 'shows' THEN COALESCE(preference.shows_enabled, true)
                                  WHEN 'releases' THEN COALESCE(preference.releases_enabled, true)
                                  WHEN 'community' THEN COALESCE(preference.community_enabled, true)
                                  WHEN 'merch' THEN COALESCE(preference.merch_enabled, true)
                                  ELSE true
                              END
                              AND NOT (
                                  COALESCE(preference.quiet_hours_enabled, false)
                                  AND CASE
                                      WHEN preference.quiet_start_minute = preference.quiet_end_minute THEN true
                                      WHEN preference.quiet_start_minute < preference.quiet_end_minute THEN
                                          ((extract(hour from now() AT TIME ZONE $4)::int * 60
                                            + extract(minute from now() AT TIME ZONE $4)::int)
                                           >= preference.quiet_start_minute
                                           AND (extract(hour from now() AT TIME ZONE $4)::int * 60
                                            + extract(minute from now() AT TIME ZONE $4)::int)
                                           < preference.quiet_end_minute)
                                      ELSE
                                          ((extract(hour from now() AT TIME ZONE $4)::int * 60
                                            + extract(minute from now() AT TIME ZONE $4)::int)
                                           >= preference.quiet_start_minute
                                           OR (extract(hour from now() AT TIME ZONE $4)::int * 60
                                            + extract(minute from now() AT TIME ZONE $4)::int)
                                           < preference.quiet_end_minute)
                                  END
                              )
                          )
                      )
                      AND (
                          (
                              delivery.audience_kind = 'staff'
                              AND (
                                  NOT EXISTS (
                                      SELECT 1
                                      FROM staff_device_sessions known_staff_session
                                      WHERE known_staff_session.workspace_id = delivery.workspace_id
                                        AND known_staff_session.token_hash = endpoint.principal_hash
                                  )
                                  OR EXISTS (
                                      SELECT 1
                                      FROM staff_device_sessions active_staff_session
                                      WHERE active_staff_session.workspace_id = delivery.workspace_id
                                        AND active_staff_session.token_hash = endpoint.principal_hash
                                        AND active_staff_session.revoked_at IS NULL
                                        AND active_staff_session.expires_at > now()
                                  )
                              )
                          )
                          OR (
                              delivery.audience_kind = 'fan'
                              AND fan.status = 'active'
                              AND COALESCE((
                                  SELECT consent.granted
                                  FROM fan_consents consent
                                  WHERE consent.workspace_id = delivery.workspace_id
                                    AND consent.fan_id = delivery.fan_id
                                    AND consent.purpose = 'marketing'
                                  ORDER BY consent.recorded_at DESC, consent.id DESC
                                  LIMIT 1
                              ), false)
                          )
                          OR (
                              delivery.audience_kind = 'beacon'
                              AND EXISTS (
                                  SELECT 1
                                  FROM viryaos_beacon_signal_sessions session
                                  JOIN viryaos_beacon_signal_profiles profile
                                    ON profile.workspace_id = session.workspace_id
                                   AND profile.beacon_id = session.beacon_id
                                   AND profile.status = 'active'
                                  JOIN viryaos_beacons beacon
                                    ON beacon.workspace_id = session.workspace_id
                                   AND beacon.id = session.beacon_id
                                   AND beacon.active
                                   AND beacon.verified
                                   AND beacon.accepts_outreach
                                   AND NOT beacon.do_not_contact
                                  WHERE session.workspace_id = delivery.workspace_id
                                    AND session.token_hash = endpoint.principal_hash
                                    AND session.revoked_at IS NULL
                                    AND session.expires_at > now()
                              )
                          )
                      )
                    ORDER BY delivery.available_at, delivery.created_at, delivery.id
                    FOR UPDATE OF delivery SKIP LOCKED
                    LIMIT $2
                ), claimed AS (
                    UPDATE fan_push_deliveries delivery
                    SET status = 'claimed', claim_token = gen_random_uuid(),
                        claimed_at = now(), attempt_count = attempt_count + 1,
                        error_code = NULL, updated_at = now()
                    FROM candidate
                    WHERE delivery.workspace_id = $1 AND delivery.id = candidate.id
                    RETURNING delivery.*
                )
                SELECT claimed.id, claimed.endpoint_id, claimed.title, claimed.body,
                       claimed.target_path, claimed.collapse_key,
                       endpoint.transport, endpoint.endpoint_address,
                       endpoint.p256dh, endpoint.auth_secret,
                       claimed.claim_token
                FROM claimed
                JOIN fan_push_endpoints endpoint
                  ON endpoint.workspace_id = claimed.workspace_id
                 AND endpoint.id = claimed.endpoint_id
                ORDER BY claimed.created_at, claimed.id
                "#,
            )
            .bind(self.workspace_id)
            .bind(limit)
            .bind(MAX_ATTEMPTS)
            .bind(&self.quiet_timezone)
            .fetch_all(&self.database),
            "claim due push deliveries",
        )
        .await
    }

    pub async fn start_provider(
        &self,
        delivery: &ClaimedDelivery,
        ack_token: &str,
    ) -> Result<bool> {
        let ack_deadline = OffsetDateTime::now_utc() + ACK_TTL;
        let affected = self
            .with_timeout(
                sqlx::query(
                    r#"
                    UPDATE fan_push_deliveries
                    SET status = 'provider_started', provider_started_at = now(),
                        ack_token_hash = digest($4, 'sha256'), ack_deadline = $5,
                        updated_at = now()
                    WHERE workspace_id = $1 AND id = $2
                      AND status = 'claimed' AND claim_token = $3
                      AND (
                          audience_kind <> 'staff'
                          OR EXISTS (
                              SELECT 1
                              FROM fan_push_endpoints endpoint
                              WHERE endpoint.workspace_id = fan_push_deliveries.workspace_id
                                AND endpoint.id = fan_push_deliveries.endpoint_id
                                AND endpoint.audience_kind = 'staff'
                                AND endpoint.active
                                AND endpoint.invalidated_at IS NULL
                                AND (
                                    NOT EXISTS (
                                        SELECT 1
                                        FROM staff_device_sessions known_staff_session
                                        WHERE known_staff_session.workspace_id = endpoint.workspace_id
                                          AND known_staff_session.token_hash = endpoint.principal_hash
                                    )
                                    OR EXISTS (
                                        SELECT 1
                                        FROM staff_device_sessions active_staff_session
                                        WHERE active_staff_session.workspace_id = endpoint.workspace_id
                                          AND active_staff_session.token_hash = endpoint.principal_hash
                                          AND active_staff_session.revoked_at IS NULL
                                          AND active_staff_session.expires_at > now()
                                    )
                                )
                          )
                      )
                    "#,
                )
                .bind(self.workspace_id)
                .bind(delivery.id)
                .bind(delivery.claim_token)
                .bind(ack_token)
                .bind(ack_deadline)
                .execute(&self.database),
                "mark push provider started",
            )
            .await?;
        Ok(affected.rows_affected() == 1)
    }

    pub async fn provider_accepted(
        &self,
        delivery_id: Uuid,
        reference: Option<&str>,
    ) -> Result<()> {
        self.with_timeout(
            sqlx::query(
                r#"
                UPDATE fan_push_deliveries
                SET status = 'provider_accepted', provider_accepted_at = now(),
                    provider_reference = $3, error_code = NULL, updated_at = now()
                WHERE workspace_id = $1 AND id = $2 AND status = 'provider_started'
                "#,
            )
            .bind(self.workspace_id)
            .bind(delivery_id)
            .bind(reference)
            .execute(&self.database),
            "mark push provider accepted",
        )
        .await?;
        Ok(())
    }

    pub async fn retry_later(&self, delivery_id: Uuid, error_code: &str) -> Result<()> {
        let delay_seconds = 30_i64;
        self.with_timeout(
            sqlx::query(
                r#"
                UPDATE fan_push_deliveries
                SET status = CASE WHEN attempt_count >= $4 THEN 'failed' ELSE 'retry_wait' END,
                    available_at = CASE WHEN attempt_count >= $4
                        THEN available_at ELSE now() + ($3::bigint * interval '1 second') END,
                    completed_at = CASE WHEN attempt_count >= $4 THEN now() ELSE NULL END,
                    error_code = $5, claim_token = NULL, claimed_at = NULL,
                    provider_started_at = NULL, ack_token_hash = NULL, ack_deadline = NULL,
                    updated_at = now()
                WHERE workspace_id = $1 AND id = $2 AND status = 'provider_started'
                "#,
            )
            .bind(self.workspace_id)
            .bind(delivery_id)
            .bind(delay_seconds)
            .bind(MAX_ATTEMPTS)
            .bind(error_code)
            .execute(&self.database),
            "schedule push retry",
        )
        .await?;
        Ok(())
    }

    pub async fn terminal(
        &self,
        delivery_id: Uuid,
        terminal: ProviderTerminal,
        error_code: &str,
        invalidate_endpoint: bool,
    ) -> Result<()> {
        let status = match terminal {
            ProviderTerminal::Failed => "failed",
            ProviderTerminal::Ambiguous => "ambiguous",
        };
        let mut transaction = self
            .database
            .begin()
            .await
            .context("push terminal transaction")?;
        let endpoint_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            UPDATE fan_push_deliveries
            SET status = $3, error_code = $4, completed_at = now(), updated_at = now()
            WHERE workspace_id = $1 AND id = $2
              AND status IN ('claimed','provider_started','provider_accepted')
            RETURNING endpoint_id
            "#,
        )
        .bind(self.workspace_id)
        .bind(delivery_id)
        .bind(status)
        .bind(error_code)
        .fetch_optional(&mut *transaction)
        .await
        .context("mark push terminal")?;
        if invalidate_endpoint && let Some(endpoint_id) = endpoint_id {
            sqlx::query(
                r#"
                    UPDATE fan_push_endpoints
                    SET active = false, invalidated_at = now(), last_error_code = $3,
                        updated_at = now()
                    WHERE workspace_id = $1 AND id = $2
                    "#,
            )
            .bind(self.workspace_id)
            .bind(endpoint_id)
            .bind(error_code)
            .execute(&mut *transaction)
            .await
            .context("invalidate push endpoint")?;
        }
        transaction.commit().await.context("commit push terminal")?;
        self.reconcile_campaigns().await
    }

    pub async fn reconcile_campaigns(&self) -> Result<()> {
        let mut transaction = self
            .database
            .begin()
            .await
            .context("push campaign reconcile transaction")?;

        sqlx::query(
            r#"
            WITH state AS (
                SELECT delivery.campaign_id, delivery.fan_id,
                       bool_or(push.status = 'delivered') AS any_delivered,
                       bool_or(push.status IN ('queued','retry_wait','claimed','provider_started','provider_accepted')) AS any_open
                FROM communication_campaign_deliveries delivery
                JOIN fan_push_deliveries push
                  ON push.workspace_id = delivery.workspace_id
                 AND push.source_kind = 'communication_campaign'
                 AND push.source_id = delivery.campaign_id
                 AND push.fan_id = delivery.fan_id
                WHERE delivery.workspace_id = $1 AND delivery.status = 'claimed'
                GROUP BY delivery.campaign_id, delivery.fan_id
            )
            UPDATE communication_campaign_deliveries delivery
            SET status = CASE WHEN state.any_delivered THEN 'delivered' ELSE 'failed' END,
                provider_reference = CASE WHEN state.any_delivered THEN 'device_ack' ELSE NULL END,
                error_code = CASE WHEN state.any_delivered THEN NULL ELSE 'push_delivery_not_acknowledged' END,
                completed_at = now(), updated_at = now()
            FROM state
            WHERE delivery.workspace_id = $1
              AND delivery.campaign_id = state.campaign_id
              AND delivery.fan_id = state.fan_id
              AND delivery.status = 'claimed'
              AND (state.any_delivered OR NOT state.any_open)
            "#,
        )
        .bind(self.workspace_id)
        .execute(&mut *transaction)
        .await
        .context("reconcile fan push campaign ledgers")?;

        let completed = sqlx::query_as::<_, (Uuid, i64, i64, i64)>(
            r#"
            WITH totals AS (
                SELECT campaign.id,
                       count(delivery.fan_id)::bigint AS recipients,
                       count(*) FILTER (WHERE delivery.status = 'delivered')::bigint AS delivered,
                       count(*) FILTER (WHERE delivery.status = 'failed')::bigint AS failed,
                       count(*) FILTER (WHERE delivery.status = 'claimed')::bigint AS open
                FROM communication_campaigns campaign
                JOIN communication_campaign_deliveries delivery
                  ON delivery.workspace_id = campaign.workspace_id
                 AND delivery.campaign_id = campaign.id
                WHERE campaign.workspace_id = $1
                  AND campaign.channel = 'push' AND campaign.status = 'scheduled'
                GROUP BY campaign.id
            )
            UPDATE communication_campaigns campaign
            SET status = 'completed', recipient_count = totals.recipients::integer,
                delivered_count = totals.delivered::integer, failed_count = totals.failed::integer,
                completed_at = now(), updated_at = now()
            FROM totals
            WHERE campaign.workspace_id = $1 AND campaign.id = totals.id
              AND totals.open = 0 AND totals.recipients = totals.delivered + totals.failed
            RETURNING campaign.id, totals.recipients, totals.delivered, totals.failed
            "#,
        )
        .bind(self.workspace_id)
        .fetch_all(&mut *transaction)
        .await
        .context("complete terminal push campaigns")?;

        if !completed.is_empty() {
            // One set-based audit write per reconcile pass; a per-campaign
            // loop would multiply statements inside the same transaction.
            let (ids, counts): (Vec<Uuid>, Vec<(i64, i64, i64)>) = completed
                .into_iter()
                .map(|(id, recipients, delivered, failed)| (id, (recipients, delivered, failed)))
                .unzip();
            let mut recipients = Vec::with_capacity(counts.len());
            let mut delivered = Vec::with_capacity(counts.len());
            let mut failed = Vec::with_capacity(counts.len());
            for (total, ok, bad) in counts {
                recipients.push(total);
                delivered.push(ok);
                failed.push(bad);
            }
            sqlx::query(
                r#"
                INSERT INTO audit_events (
                    workspace_id, actor_kind, action, target_type, target_id, metadata
                )
                SELECT $1, 'system', 'communication.campaign.push.completed',
                       'communication_campaign', completed.campaign_id::text,
                       jsonb_build_object(
                           'recipient_count', recipient_count,
                           'delivered_count', delivered_count,
                           'failed_count', failed_count
                       )
                FROM unnest($2::uuid[], $3::bigint[], $4::bigint[], $5::bigint[])
                     AS completed(campaign_id, recipient_count, delivered_count, failed_count)
                "#,
            )
            .bind(self.workspace_id)
            .bind(&ids)
            .bind(&recipients)
            .bind(&delivered)
            .bind(&failed)
            .execute(&mut *transaction)
            .await
            .context("audit completed push campaigns")?;
        }

        transaction
            .commit()
            .await
            .context("commit push campaign reconcile")?;
        Ok(())
    }

    async fn with_timeout<T, F>(&self, future: F, label: &'static str) -> Result<T>
    where
        F: std::future::Future<Output = Result<T, sqlx::Error>>,
    {
        tokio::time::timeout(self.operation_timeout, future)
            .await
            .with_context(|| format!("{label} timed out"))?
            .with_context(|| label.to_string())
    }
}

fn seconds_i64(value: Duration) -> i64 {
    i64::try_from(value.as_secs()).unwrap_or(i64::MAX)
}
