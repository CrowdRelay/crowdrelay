async fn load_signal_summary(state: &OpsState) -> Result<SignalSummaryRow, OpsError> {
    sqlx::query_as::<_, SignalSummaryRow>(
        r#"
        WITH latest_marketing AS (
            SELECT DISTINCT ON (fan_id)
                   fan_id, granted
            FROM fan_consents
            WHERE workspace_id = $1
              AND purpose = 'marketing'
            ORDER BY fan_id, recorded_at DESC, id DESC
        ),
        fan_summary AS (
            SELECT
                count(*) AS total_fans,
                count(*) FILTER (WHERE status = 'active') AS active_fans,
                count(*) FILTER (WHERE status = 'pending') AS pending_fans,
                count(*) FILTER (WHERE status = 'unsubscribed') AS unsubscribed_fans,
                count(*) FILTER (WHERE status = 'suppressed') AS suppressed_fans,
                count(*) FILTER (
                    WHERE status = 'active'
                      AND created_at >= now() - interval '7 days'
                ) AS new_fans_7d,
                count(*) FILTER (
                    WHERE status = 'active'
                      AND created_at >= now() - interval '30 days'
                ) AS new_fans_30d
            FROM fans
            WHERE workspace_id = $1
        ),
        consent_summary AS (
            SELECT count(*) FILTER (
                WHERE consent.granted
                  AND fan.status = 'active'
            ) AS marketing_opted_in
            FROM latest_marketing AS consent
            JOIN fans AS fan
              ON fan.workspace_id = $1
             AND fan.id = consent.fan_id
        ),
        location_summary AS (
            SELECT
                count(*) FILTER (
                    WHERE preference.nearby_gigs_enabled
                      AND fan.status = 'active'
                ) AS nearby_enabled,
                count(DISTINCT preference.city_id) FILTER (
                    WHERE city.moderation_status = 'pending'
                      AND fan.status = 'active'
                ) AS pending_city_requests,
                -- Moderation and coordinates are different blockers and only
                -- one of them stops delivery. The nearby-show query gates on
                -- latitude, not on `moderation_status`, so a city can be
                -- awaiting a human and still reach its fans -- and a city can
                -- be approved and reach nobody. Counting only the moderation
                -- queue hid exactly the failure that matters.
                count(DISTINCT preference.city_id) FILTER (
                    WHERE city.latitude IS NULL
                      AND fan.status = 'active'
                ) AS cities_awaiting_coordinates,
                count(DISTINCT preference.city_id) FILTER (
                    WHERE city.latitude IS NOT NULL
                      AND fan.status = 'active'
                ) AS cities_resolved,
                count(*) FILTER (
                    WHERE city.latitude IS NOT NULL
                      AND fan.status = 'active'
                ) AS fans_with_coordinates,
                -- What the loop can actually reach right now: the same three
                -- conditions `emit_due_nearby_gigs` applies before it will
                -- announce anything to a fan.
                count(*) FILTER (
                    WHERE preference.nearby_gigs_enabled
                      AND fan.status = 'active'
                      AND city.latitude IS NOT NULL
                      AND consent.granted
                ) AS nearby_eligible_fans
            FROM fan_location_preferences AS preference
            JOIN fans AS fan
              ON fan.workspace_id = preference.workspace_id
             AND fan.id = preference.fan_id
            JOIN cities AS city
              ON city.id = preference.city_id
            LEFT JOIN latest_marketing AS consent
              ON consent.fan_id = preference.fan_id
            WHERE preference.workspace_id = $1
        ),
        push_summary AS (
            SELECT
                count(*) FILTER (
                    WHERE status IN ('queued', 'claimed', 'retry_wait')
                ) AS pushes_queued,
                -- Accepted by the provider, which is as far as "sent" can be
                -- known from here. `delivered` is the app's own acknowledgement
                -- that the notification reached the device, so the gap between
                -- the two is the part of the loop nobody else reports on.
                count(*) FILTER (
                    WHERE provider_accepted_at IS NOT NULL
                ) AS pushes_sent,
                count(*) FILTER (WHERE status = 'delivered') AS pushes_delivered,
                count(*) FILTER (
                    WHERE status IN ('failed', 'ambiguous')
                ) AS pushes_failed
            FROM fan_push_deliveries
            WHERE workspace_id = $1
        ),
        referral_summary AS (
            SELECT
                count(*) AS referral_attributions_total,
                count(*) FILTER (
                    WHERE accepted_at >= now() - interval '30 days'
                ) AS referral_attributions_30d
            FROM referral_attributions
            WHERE workspace_id = $1
        ),
        interest_summary AS (
            SELECT
                count(*) AS event_interests_total,
                count(*) FILTER (
                    WHERE created_at >= now() - interval '30 days'
                ) AS event_interests_30d
            FROM event_interests
            WHERE workspace_id = $1
        ),
        notification_summary AS (
            SELECT
                count(*) FILTER (
                    WHERE created_at >= now() - interval '30 days'
                ) AS nearby_notifications_30d,
                count(*) AS nearby_notifications_total
            FROM nearby_gig_notifications
            WHERE workspace_id = $1
        )
        SELECT
            fan_summary.total_fans,
            fan_summary.active_fans,
            fan_summary.pending_fans,
            fan_summary.unsubscribed_fans,
            fan_summary.suppressed_fans,
            consent_summary.marketing_opted_in,
            location_summary.nearby_enabled,
            fan_summary.new_fans_7d,
            fan_summary.new_fans_30d,
            referral_summary.referral_attributions_total,
            referral_summary.referral_attributions_30d,
            interest_summary.event_interests_total,
            interest_summary.event_interests_30d,
            notification_summary.nearby_notifications_30d,
            location_summary.pending_city_requests,
            location_summary.cities_awaiting_coordinates,
            location_summary.cities_resolved,
            location_summary.fans_with_coordinates,
            location_summary.nearby_eligible_fans,
            notification_summary.nearby_notifications_total,
            push_summary.pushes_queued,
            push_summary.pushes_sent,
            push_summary.pushes_delivered,
            push_summary.pushes_failed
        FROM fan_summary
        CROSS JOIN consent_summary
        CROSS JOIN location_summary
        CROSS JOIN referral_summary
        CROSS JOIN interest_summary
        CROSS JOIN notification_summary
        CROSS JOIN push_summary
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_one(&state.pool)
    .await
    .map_err(OpsError::sqlx)
}

async fn load_signal_top_cities(state: &OpsState) -> Result<Vec<SignalCitySummary>, OpsError> {
    sqlx::query_as::<_, SignalCitySummary>(
        r#"
        SELECT
            city.slug,
            city.name,
            city.country_code::text AS country_code,
            count(DISTINCT fan.id) AS active_fans
        FROM fan_city_interests AS interest
        JOIN fans AS fan
          ON fan.workspace_id = interest.workspace_id
         AND fan.id = interest.fan_id
        JOIN cities AS city
          ON city.id = interest.city_id
        WHERE interest.workspace_id = $1
          AND fan.status = 'active'
        GROUP BY city.slug, city.name, city.country_code
        ORDER BY active_fans DESC, city.name ASC, city.slug ASC
        LIMIT 10
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_all(&state.pool)
    .await
    .map_err(OpsError::sqlx)
}

async fn load_summary(state: &OpsState) -> Result<OpsSummary, OpsError> {
    let row = sqlx::query_as::<_, OpsSummaryRow>(
        r#"
        WITH outbox AS (
            SELECT
                count(*) FILTER (WHERE status = 'pending')::bigint AS pending,
                count(*) FILTER (WHERE status = 'processing')::bigint AS processing,
                count(*) FILTER (
                    WHERE status = 'delivered'
                      AND delivered_at >= now() - interval '24 hours'
                )::bigint AS delivered_24h,
                count(*) FILTER (WHERE status = 'dead')::bigint AS dead,
                COALESCE(EXTRACT(EPOCH FROM (now() - min(available_at) FILTER (
                    WHERE status = 'pending' AND available_at <= now()
                )))::bigint, 0) AS oldest_pending_seconds
            FROM outbox_events
            WHERE workspace_id = $1
        ),
        deliveries AS (
            SELECT
                count(*) FILTER (WHERE status = 'pending')::bigint AS pending,
                count(*) FILTER (WHERE status = 'processing')::bigint AS processing,
                count(*) FILTER (
                    WHERE status = 'delivered'
                      AND delivered_at >= now() - interval '24 hours'
                )::bigint AS delivered_24h,
                count(*) FILTER (WHERE status = 'dead')::bigint AS dead,
                count(*) FILTER (WHERE status = 'cancelled')::bigint AS cancelled,
                COALESCE(EXTRACT(EPOCH FROM (now() - min(available_at) FILTER (
                    WHERE status = 'pending' AND available_at <= now()
                )))::bigint, 0) AS oldest_pending_seconds
            FROM webhook_deliveries
            WHERE workspace_id = $1
        ),
        push AS (
            SELECT
                count(*) FILTER (WHERE status IN ('queued','retry_wait'))::bigint AS pending,
                count(*) FILTER (WHERE status IN ('claimed','provider_started','provider_accepted'))::bigint AS processing,
                count(*) FILTER (WHERE status = 'delivered' AND delivered_at >= now() - interval '24 hours')::bigint AS delivered_24h,
                count(*) FILTER (WHERE status IN ('failed','ambiguous') AND error_code IS DISTINCT FROM 'preference_disabled')::bigint AS dead,
                count(*) FILTER (WHERE status = 'failed' AND error_code = 'preference_disabled')::bigint AS suppressed,
                COALESCE(EXTRACT(EPOCH FROM (now() - min(available_at) FILTER (
                    WHERE status IN ('queued','retry_wait') AND available_at <= now()
                )))::bigint, 0) AS oldest_pending_seconds
            FROM fan_push_deliveries
            WHERE workspace_id = $1
        )
        SELECT
            outbox.pending AS outbox_pending,
            outbox.processing AS outbox_processing,
            outbox.delivered_24h AS outbox_delivered_24h,
            outbox.dead AS outbox_dead,
            outbox.oldest_pending_seconds AS outbox_oldest_pending_seconds,
            deliveries.pending AS delivery_pending,
            deliveries.processing AS delivery_processing,
            deliveries.delivered_24h AS delivery_delivered_24h,
            deliveries.dead AS delivery_dead,
            deliveries.cancelled AS delivery_cancelled,
            deliveries.oldest_pending_seconds AS delivery_oldest_pending_seconds,
            push.pending AS push_pending,
            push.processing AS push_processing,
            push.delivered_24h AS push_delivered_24h,
            push.dead AS push_dead,
            push.suppressed AS push_suppressed,
            push.oldest_pending_seconds AS push_oldest_pending_seconds
        FROM outbox CROSS JOIN deliveries CROSS JOIN push
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_one(&state.pool)
    .await
    .map_err(OpsError::sqlx)?;
    let watchdog =
        crate::ops_summary::load_watchdog_summary(&state.pool, state.workspace_id.into_uuid())
            .await
            .map_err(OpsError::sqlx)?;
    let worker = crate::ops_summary::load_worker_summary(&state.pool)
        .await
        .map_err(OpsError::sqlx)?;

    let database = sqlx::query_as::<_, DatabaseRuntimeRow>(
        r#"
        SELECT
            current_setting('server_version_num')::integer AS server_version_num,
            current_setting('io_method', true) AS io_method,
            NULLIF(current_setting('io_workers', true), '')::integer AS io_workers,
            NULLIF(current_setting('io_max_concurrency', true), '')::integer AS io_max_concurrency,
            NULLIF(current_setting('effective_io_concurrency', true), '')::integer
                AS effective_io_concurrency,
            NULLIF(current_setting('maintenance_io_concurrency', true), '')::integer
                AS maintenance_io_concurrency,
            CASE
                WHEN NULLIF(current_setting('io_combine_limit', true), '') IS NULL THEN NULL
                ELSE pg_size_bytes(current_setting('io_combine_limit', true))::bigint
            END AS io_combine_limit_bytes,
            CASE
                WHEN NULLIF(current_setting('io_max_combine_limit', true), '') IS NULL THEN NULL
                ELSE pg_size_bytes(current_setting('io_max_combine_limit', true))::bigint
            END AS io_max_combine_limit_bytes,
            -- The newest migration this database has applied.
            --
            -- `schema_version` below is the build's constant, baked in from the
            -- migrations directory at compile time: it describes the binary.
            -- The two can disagree, and when they did nobody could tell --
            -- production served `schema_version: 234` while the database had
            -- 236 applied, and a migration that had landed ahead of its code
            -- silently changed how one query behaved. Reporting both is the
            -- cheapest way for that to be visible where an operator looks.
            (SELECT max(version) FROM _sqlx_migrations)::bigint
                AS database_schema_version
        "#,
    )
    .fetch_one(&state.pool)
    .await
    .map_err(OpsError::sqlx)?;
    let area = sqlx::query_as::<_, AreaRuntimeRow>(
        r#"
        SELECT
            COALESCE((SELECT sum(delta)::bigint FROM area_credit_ledger WHERE workspace_id = $1), 0) AS credits_total,
            COALESCE((SELECT count(*)::bigint FROM area_reward_vouchers WHERE workspace_id = $1 AND status = 'issued'), 0) AS vouchers_issued,
            COALESCE((SELECT count(*)::bigint FROM area_reward_vouchers WHERE workspace_id = $1 AND status = 'reserved' AND reserved_until < now()), 0) AS stale_voucher_reservations,
            COALESCE((SELECT count(*)::bigint FROM area_ticket_rewards WHERE workspace_id = $1 AND status = 'issued'), 0) AS ticket_rewards_issued,
            COALESCE((SELECT count(*)::bigint FROM area_ticket_rewards WHERE workspace_id = $1 AND status = 'reserved' AND reservation_expires_at < now()), 0) AS stale_ticket_reward_reservations,
            COALESCE((SELECT count(*)::bigint FROM area_legacy_wallet_imports WHERE workspace_id = $1), 0) AS legacy_imported_players
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_one(&state.pool)
    .await
    .map_err(OpsError::sqlx)?;

    let database = DatabaseRuntimeSummary::from_row(&state.pool, database);

    Ok(OpsSummary {
        outbox: QueueSummary {
            pending: row.outbox_pending,
            processing: row.outbox_processing,
            delivered_24h: row.outbox_delivered_24h,
            dead: row.outbox_dead,
            cancelled: 0,
            oldest_pending_seconds: row.outbox_oldest_pending_seconds,
        },
        deliveries: QueueSummary {
            pending: row.delivery_pending,
            processing: row.delivery_processing,
            delivered_24h: row.delivery_delivered_24h,
            dead: row.delivery_dead,
            cancelled: row.delivery_cancelled,
            oldest_pending_seconds: row.delivery_oldest_pending_seconds,
        },
        push: QueueSummary {
            pending: row.push_pending,
            processing: row.push_processing,
            delivered_24h: row.push_delivered_24h,
            dead: row.push_dead,
            cancelled: row.push_suppressed,
            oldest_pending_seconds: row.push_oldest_pending_seconds,
        },
        watchdog,
        worker,
        http: http_request_summary(crate::http_metrics().snapshot()),
        database,
        area: AreaRuntimeSummary {
            credits_total: area.credits_total,
            vouchers_issued: area.vouchers_issued,
            stale_voucher_reservations: area.stale_voucher_reservations,
            ticket_rewards_issued: area.ticket_rewards_issued,
            stale_ticket_reward_reservations: area.stale_ticket_reward_reservations,
            legacy_imported_players: area.legacy_imported_players,
        },
        schema_version: crate::meta::SCHEMA_VERSION,
        release: option_env!("CROWDRELAY_RELEASE")
            .unwrap_or(env!("CARGO_PKG_VERSION"))
            .to_owned(),
    })
}

fn http_request_summary(snapshot: crate::http_metrics::HttpMetricsSnapshot) -> HttpRequestSummary {
    let average_ms = snapshot
        .latency_micros_sum
        .checked_div(snapshot.total)
        .unwrap_or_default()
        / 1_000;
    HttpRequestSummary {
        requests: snapshot.total,
        errors_4xx: snapshot.errors_4xx,
        errors_5xx: snapshot.errors_5xx,
        average_ms,
        p50_ms: percentile_bucket_ms(snapshot, 50),
        p95_ms: percentile_bucket_ms(snapshot, 95),
    }
}

fn percentile_bucket_ms(
    snapshot: crate::http_metrics::HttpMetricsSnapshot,
    percentile: u64,
) -> u64 {
    if snapshot.total == 0 {
        return 0;
    }
    let target = snapshot.total.saturating_mul(percentile).div_ceil(100);
    for (bound, count) in [
        (50, snapshot.le_50_ms),
        (100, snapshot.le_100_ms),
        (250, snapshot.le_250_ms),
        (500, snapshot.le_500_ms),
        (1_000, snapshot.le_1000_ms),
        (2_500, snapshot.le_2500_ms),
        (5_000, snapshot.le_5000_ms),
    ] {
        if count >= target {
            return bound;
        }
    }
    5_001
}

async fn load_metrics_snapshot(state: &OpsState) -> Result<OpsMetricsSnapshot, OpsError> {
    // Prometheus scrapes do not need the 24-hour delivered counters used by the
    // admin summary. Keep this query narrow to reduce CPU and buffer churn.
    let row = sqlx::query_as::<_, OpsMetricsRow>(
        r#"
        WITH outbox AS (
            SELECT
                count(*) FILTER (WHERE status = 'pending')::bigint AS pending,
                count(*) FILTER (WHERE status = 'processing')::bigint AS processing,
                count(*) FILTER (WHERE status = 'dead')::bigint AS dead,
                COALESCE(EXTRACT(EPOCH FROM (now() - min(available_at) FILTER (
                    WHERE status = 'pending' AND available_at <= now()
                )))::bigint, 0) AS oldest_pending_seconds
            FROM outbox_events
            WHERE workspace_id = $1
        ),
        deliveries AS (
            SELECT
                count(*) FILTER (WHERE status = 'pending')::bigint AS pending,
                count(*) FILTER (WHERE status = 'processing')::bigint AS processing,
                count(*) FILTER (WHERE status = 'dead')::bigint AS dead,
                count(*) FILTER (WHERE status = 'cancelled')::bigint AS cancelled,
                COALESCE(EXTRACT(EPOCH FROM (now() - min(available_at) FILTER (
                    WHERE status = 'pending' AND available_at <= now()
                )))::bigint, 0) AS oldest_pending_seconds
            FROM webhook_deliveries
            WHERE workspace_id = $1
        ),
        push AS (
            SELECT
                count(*) FILTER (WHERE status IN ('queued','retry_wait'))::bigint AS pending,
                count(*) FILTER (WHERE status IN ('claimed','provider_started','provider_accepted'))::bigint AS processing,
                count(*) FILTER (WHERE status IN ('failed','ambiguous') AND error_code IS DISTINCT FROM 'preference_disabled')::bigint AS dead,
                count(*) FILTER (WHERE status = 'failed' AND error_code = 'preference_disabled')::bigint AS suppressed,
                COALESCE(EXTRACT(EPOCH FROM (now() - min(available_at) FILTER (
                    WHERE status IN ('queued','retry_wait') AND available_at <= now()
                )))::bigint, 0) AS oldest_pending_seconds
            FROM fan_push_deliveries
            WHERE workspace_id = $1
        ),
        -- Worker liveness, read from the leadership lease.
        --
        -- The worker exposes no HTTP surface, so Prometheus cannot scrape it
        -- and `up{job=...}` does not exist for it. It does renew this lease
        -- every 15 seconds, which makes the lease age the one honest heartbeat
        -- available — and the API, which is scraped, can read it.
        --
        -- Without this the process running the entire brain, all outbox
        -- delivery and every metric sync could die unnoticed. It did: killed by
        -- a deploy, dead for over fifteen minutes, and nothing said so.
        --
        -- Not workspace-scoped; leadership is per deployment, not per tenant.
        -- No row at all reads as maximally stale rather than as healthy.
        worker AS (
            SELECT COALESCE((
                SELECT EXTRACT(EPOCH FROM (
                    now() - (expires_at - INTERVAL '60 seconds')
                ))::bigint
                FROM worker_leadership WHERE id = 1
            ), 999999) AS lease_age_seconds
        )
        SELECT
            outbox.pending AS outbox_pending,
            outbox.processing AS outbox_processing,
            outbox.dead AS outbox_dead,
            outbox.oldest_pending_seconds AS outbox_oldest_pending_seconds,
            deliveries.pending AS delivery_pending,
            deliveries.processing AS delivery_processing,
            deliveries.dead AS delivery_dead,
            deliveries.cancelled AS delivery_cancelled,
            deliveries.oldest_pending_seconds AS delivery_oldest_pending_seconds,
            push.pending AS push_pending,
            push.processing AS push_processing,
            push.dead AS push_dead,
            push.suppressed AS push_suppressed,
            push.oldest_pending_seconds AS push_oldest_pending_seconds,
            worker.lease_age_seconds AS worker_lease_age_seconds
        FROM outbox CROSS JOIN deliveries CROSS JOIN push CROSS JOIN worker
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .fetch_one(&state.pool)
    .await
    .map_err(OpsError::sqlx)?;

    Ok(OpsMetricsSnapshot {
        outbox_pending: row.outbox_pending,
        outbox_processing: row.outbox_processing,
        outbox_dead: row.outbox_dead,
        outbox_oldest_pending_seconds: row.outbox_oldest_pending_seconds,
        delivery_pending: row.delivery_pending,
        delivery_processing: row.delivery_processing,
        delivery_dead: row.delivery_dead,
        delivery_cancelled: row.delivery_cancelled,
        delivery_oldest_pending_seconds: row.delivery_oldest_pending_seconds,
        push_pending: row.push_pending,
        push_processing: row.push_processing,
        push_dead: row.push_dead,
        push_suppressed: row.push_suppressed,
        push_oldest_pending_seconds: row.push_oldest_pending_seconds,
        worker_lease_age_seconds: row.worker_lease_age_seconds,
    })
}

async fn load_delivery(state: &OpsState, id: Uuid) -> Result<Option<DeliveryDetails>, OpsError> {
    let delivery = sqlx::query_as::<_, DeliveryItem>(
        r#"
        SELECT delivery.id, delivery.outbox_event_id, event.event_type,
               endpoint.name AS endpoint_name, endpoint.active AS endpoint_active,
               delivery.status, delivery.attempt_count, delivery.max_attempts,
               delivery.available_at, delivery.last_response_status,
               delivery.last_error_kind, delivery.created_at, delivery.updated_at,
               delivery.delivered_at, delivery.dead_at
        FROM webhook_deliveries AS delivery
        JOIN outbox_events AS event
          ON event.workspace_id = delivery.workspace_id
         AND event.id = delivery.outbox_event_id
        JOIN webhook_endpoints AS endpoint
          ON endpoint.workspace_id = delivery.workspace_id
         AND endpoint.id = delivery.endpoint_id
        WHERE delivery.workspace_id = $1 AND delivery.id = $2
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(OpsError::sqlx)?;
    let Some(delivery) = delivery else {
        return Ok(None);
    };
    let attempts = sqlx::query_as::<_, DeliveryAttempt>(
        r#"
        SELECT attempt_number, started_at, finished_at, outcome,
               response_status, error_kind, duration_ms
        FROM webhook_delivery_attempts
        WHERE workspace_id = $1 AND delivery_id = $2
        ORDER BY attempt_number DESC
        LIMIT 100
        "#,
    )
    .bind(state.workspace_id.into_uuid())
    .bind(id)
    .fetch_all(&state.pool)
    .await
    .map_err(OpsError::sqlx)?;
    Ok(Some(DeliveryDetails { delivery, attempts }))
}

fn page_size(limit: Option<i64>) -> Result<i64, OpsError> {
    let limit = limit.unwrap_or(DEFAULT_PAGE_SIZE);
    (1..=MAX_PAGE_SIZE)
        .contains(&limit)
        .then_some(limit)
        .ok_or(OpsError::BadRequest)
}

fn parse_id(id: &str) -> Result<Uuid, OpsError> {
    Uuid::parse_str(id).map_err(|_| OpsError::BadRequest)
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, OpsError> {
    let value = headers
        .get(&IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| {
            (8..=128).contains(&value.len())
                && value.bytes().all(|byte| (b'!'..=b'~').contains(&byte))
        })
        .ok_or(OpsError::BadRequest)?;
    Ok(value.to_owned())
}

async fn run_with_timeout<T, E>(
    duration: Duration,
    future: impl Future<Output = Result<T, E>>,
) -> Result<T, OpsError>
where
    E: Into<OpsError>,
{
    timeout(duration, future)
        .await
        .map_err(|_| OpsError::Unavailable)?
        .map_err(Into::into)
}

fn private_json<T: Serialize>(status: StatusCode, body: T) -> Response {
    (status, [(CACHE_CONTROL, PRIVATE_NO_STORE)], Json(body)).into_response()
}

#[derive(Debug)]
pub(crate) enum OpsError {
    BadRequest,
    NotFound,
    Conflict,
    InactiveEndpoint,
    Unavailable,
    Unexpected,
}

impl OpsError {
    fn sqlx(error: sqlx::Error) -> Self {
        tracing::error!(error = %error, "operations database query failed");
        Self::Unexpected
    }

    fn into_response(self, request_id: Option<String>) -> Response {
        match self {
            Self::BadRequest => Problem::bad_request(request_id).private().into_response(),
            Self::NotFound => Problem::not_found(request_id).private().into_response(),
            Self::Conflict | Self::InactiveEndpoint => {
                Problem::conflict(request_id).private().into_response()
            }
            Self::Unavailable => Problem::service_unavailable(request_id)
                .private()
                .into_response(),
            Self::Unexpected => Problem::internal(request_id).private().into_response(),
        }
    }
}

impl From<sqlx::Error> for OpsError {
    fn from(error: sqlx::Error) -> Self {
        Self::sqlx(error)
    }
}

#[cfg(test)]
mod signal_tests {
    use super::{SignalCitySummary, SignalSummaryRow, signal_overview_from_row};

    #[test]
    fn signal_overview_payload_is_aggregate_only() {
        let overview = signal_overview_from_row(
            SignalSummaryRow {
                total_fans: 14,
                active_fans: 10,
                pending_fans: 2,
                unsubscribed_fans: 1,
                suppressed_fans: 1,
                marketing_opted_in: 9,
                nearby_enabled: 8,
                new_fans_7d: 3,
                new_fans_30d: 6,
                referral_attributions_total: 11,
                referral_attributions_30d: 4,
                event_interests_total: 20,
                event_interests_30d: 7,
                nearby_notifications_30d: 5,
                pending_city_requests: 1,
                cities_awaiting_coordinates: 2,
                cities_resolved: 3,
                fans_with_coordinates: 7,
                nearby_eligible_fans: 6,
                nearby_notifications_total: 12,
                pushes_queued: 4,
                pushes_sent: 9,
                pushes_delivered: 8,
                pushes_failed: 1,
            },
            vec![SignalCitySummary {
                slug: "wroclaw".to_owned(),
                name: "Wrocław".to_owned(),
                country_code: "PL".to_owned(),
                active_fans: 8,
            }],
            Vec::new(),
        );

        let json = match serde_json::to_string(&overview) {
            Ok(json) => json,
            Err(error) => panic!("signal overview serialization failed: {error}"),
        };
        assert!(json.contains("\"active_fans\":10"));
        assert!(json.contains("\"top_cities\""));
        // The retention loop is only debuggable if every stage between a
        // requested city and a delivered push is reported. Losing any one of
        // them puts the loop back to failing silently.
        for stage in [
            "\"cities_awaiting_coordinates\":2",
            "\"cities_resolved\":3",
            "\"fans_with_coordinates\":7",
            "\"nearby_eligible_fans\":6",
            "\"notifications_created\":12",
            "\"pushes_queued\":4",
            "\"pushes_sent\":9",
            "\"pushes_delivered\":8",
            "\"pushes_failed\":1",
        ] {
            assert!(json.contains(stage), "missing retention stage {stage}");
        }
        assert!(!json.contains("email"));
        assert!(!json.contains("display_name"));
        assert!(!json.contains("fan_id"));
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::{OpsError, idempotency_key, page_size, parse_id};

    #[test]
    fn pagination_is_bounded() {
        assert_eq!(page_size(None).ok(), Some(50));
        assert_eq!(page_size(Some(1)).ok(), Some(1));
        assert_eq!(page_size(Some(100)).ok(), Some(100));
        assert!(matches!(page_size(Some(0)), Err(OpsError::BadRequest)));
        assert!(matches!(page_size(Some(101)), Err(OpsError::BadRequest)));
    }

    #[test]
    fn retry_identifiers_and_keys_are_strict() {
        assert!(parse_id("0198f120-f478-7d55-b1b8-5f3a4118dc75").is_ok());
        assert!(matches!(parse_id("not-a-uuid"), Err(OpsError::BadRequest)));

        let mut headers = HeaderMap::new();
        assert!(matches!(
            idempotency_key(&headers),
            Err(OpsError::BadRequest)
        ));
        headers.insert(
            "idempotency-key",
            HeaderValue::from_static("ops-retry-0198f120"),
        );
        assert_eq!(
            idempotency_key(&headers).ok().as_deref(),
            Some("ops-retry-0198f120")
        );
        headers.insert(
            "idempotency-key",
            HeaderValue::from_static("retry with spaces"),
        );
        assert!(matches!(
            idempotency_key(&headers),
            Err(OpsError::BadRequest)
        ));
    }
}
