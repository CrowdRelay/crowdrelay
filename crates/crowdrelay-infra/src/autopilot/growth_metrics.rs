//! PostgreSQL adapter for external growth metrics.
//!
//! Two writes and two reads. The writes are ordinary idempotent operator
//! ingress; the reads are set-oriented on purpose — one query for the series,
//! one for their observation window, one for the last signal per series — so
//! adding a tracked metric costs rows, never round trips.
//!
//! No trend arithmetic happens in SQL. Postgres returns observations and the
//! domain derives the movement, so the read model an operator sees and the
//! evidence a decision is made from can never drift apart.

use super::control::insert_operator_action;
use super::*;
use crowdrelay_domain::growth_metrics::MetricTrend;

/// Observations older than the 28-day window are still needed: the 28-day
/// comparison point is allowed a 7-day tolerance, so its floor sits at 35 days.
const WINDOW_DAYS: i64 = 35;

#[derive(Debug, FromRow)]
struct SeriesRow {
    id: Uuid,
    platform: String,
    metric_key: String,
    display_name: String,
    subject_kind: Option<String>,
    subject_id: Option<Uuid>,
    direction: String,
    value_tier: String,
    expected_interval_hours: i32,
}

#[derive(Debug, FromRow)]
struct PointRow {
    series_id: Uuid,
    captured_at: OffsetDateTime,
    value: i64,
}

#[derive(Debug, FromRow)]
struct LastSignalRow {
    subject_id: Uuid,
    evaluated_at: OffsetDateTime,
}

/// One series with everything derived from its own observations.
struct LoadedSeries {
    row: SeriesRow,
    platform: MetricPlatform,
    direction: MetricDirection,
    value_tier: MetricValueTier,
    expected_interval_hours: u32,
    trend: MetricTrend,
    hours_since_last_signal: Option<u32>,
    stronger_tier_tracked: bool,
}

fn parse_series_enums(
    row: &SeriesRow,
) -> Result<(MetricPlatform, MetricDirection, MetricValueTier, u32), RepositoryError> {
    let platform = MetricPlatform::parse(&row.platform).ok_or(RepositoryError::Unexpected)?;
    let direction = MetricDirection::parse(&row.direction).ok_or(RepositoryError::Unexpected)?;
    let value_tier = MetricValueTier::parse(&row.value_tier).ok_or(RepositoryError::Unexpected)?;
    let interval =
        u32::try_from(row.expected_interval_hours).map_err(|_| RepositoryError::Unexpected)?;
    Ok((platform, direction, value_tier, interval))
}

impl PostgresAutopilotRepository {
    /// Shared loader for both the evaluator and the operator read model.
    async fn load_series(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<LoadedSeries>, RepositoryError> {
        let workspace = workspace_id.into_uuid();
        let series = sqlx::query_as::<_, SeriesRow>(
            r#"
            SELECT id, platform, metric_key, display_name, subject_kind, subject_id,
                   direction, value_tier, expected_interval_hours
            FROM viryaos_growth_metric_series
            WHERE workspace_id = $1 AND active
            ORDER BY platform, metric_key, id
            "#,
        )
        .bind(workspace)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;
        if series.is_empty() {
            return Ok(Vec::new());
        }

        let points = sqlx::query_as::<_, PointRow>(
            r#"
            SELECT point.series_id, point.captured_at, point.value
            FROM viryaos_growth_metric_points AS point
            JOIN viryaos_growth_metric_series AS series
              ON series.workspace_id = point.workspace_id
             AND series.id = point.series_id
             AND series.active
            WHERE point.workspace_id = $1
              AND point.captured_at >= $2 - make_interval(days => $3::int)
              AND point.captured_at <= $2
            ORDER BY point.series_id, point.captured_at
            "#,
        )
        .bind(workspace)
        .bind(now)
        .bind(i32::try_from(WINDOW_DAYS).unwrap_or(35))
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;

        // The cooldown is expressed against the last decision this context made
        // about the series, so a finding that was already surfaced is not
        // surfaced again on the next cycle.
        let last_signals = sqlx::query_as::<_, LastSignalRow>(
            r#"
            SELECT subject_id, max(evaluated_at) AS evaluated_at
            FROM viryaos_autopilot_decisions
            WHERE workspace_id = $1
              AND context = 'growth_metrics'
              AND subject_kind = 'growth_metric_series'
            GROUP BY subject_id
            "#,
        )
        .bind(workspace)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;

        let mut grouped: HashMap<Uuid, Vec<MetricPoint>> = HashMap::new();
        for point in points {
            grouped
                .entry(point.series_id)
                .or_default()
                .push(MetricPoint {
                    captured_at: point.captured_at,
                    value: point.value,
                });
        }
        let last_signal_at: HashMap<Uuid, OffsetDateTime> = last_signals
            .into_iter()
            .map(|row| (row.subject_id, row.evaluated_at))
            .collect();

        // "A stronger metric is already tracked for this platform" is a fact
        // about the set, so it is derived once here rather than re-queried per
        // series.
        let mut strongest_tier: HashMap<String, MetricValueTier> = HashMap::new();
        for row in &series {
            let Some(tier) = MetricValueTier::parse(&row.value_tier) else {
                continue;
            };
            strongest_tier
                .entry(row.platform.clone())
                .and_modify(|current| {
                    if tier > *current {
                        *current = tier;
                    }
                })
                .or_insert(tier);
        }

        let mut loaded = Vec::with_capacity(series.len());
        for row in series {
            let (platform, direction, value_tier, expected_interval_hours) =
                parse_series_enums(&row)?;
            let Some(points) = grouped.get(&row.id) else {
                // A declared series with no observation in the window has
                // nothing to describe. Reporting a zeroed trend would invent a
                // flat line that was never measured.
                continue;
            };
            let Some(trend) = compute_trend(points, now) else {
                continue;
            };
            let hours_since_last_signal = last_signal_at
                .get(&row.id)
                .map(|at| u32::try_from((now - *at).whole_hours().max(0)).unwrap_or(u32::MAX));
            let stronger_tier_tracked = strongest_tier
                .get(&row.platform)
                .is_some_and(|strongest| *strongest > value_tier);
            loaded.push(LoadedSeries {
                row,
                platform,
                direction,
                value_tier,
                expected_interval_hours,
                trend,
                hours_since_last_signal,
                stronger_tier_tracked,
            });
        }
        Ok(loaded)
    }

    /// Reads the workspace's `growth_metrics` thresholds so the operator view
    /// and the evaluator agree on what "stale" means.
    async fn growth_metric_policy(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<GrowthMetricPolicy, RepositoryError> {
        let config = sqlx::query_scalar::<_, Value>(
            r#"
            SELECT config
            FROM viryaos_autopilot_policies
            WHERE workspace_id = $1 AND context = 'growth_metrics'
            "#,
        )
        .bind(workspace_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(config
            .map(|value| serde_json::from_value(value).unwrap_or_default())
            .unwrap_or_default())
    }

    pub(super) async fn load_growth_metric_snapshots_impl(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<GrowthMetricSnapshot>, RepositoryError> {
        self.bounded(async {
            let loaded = self.load_series(workspace_id, now).await?;
            Ok(loaded
                .into_iter()
                .map(|series| GrowthMetricSnapshot {
                    series_id: GrowthMetricSeriesId::from_uuid(series.row.id),
                    platform: series.platform,
                    metric_key: series.row.metric_key,
                    direction: series.direction,
                    value_tier: series.value_tier,
                    expected_interval_hours: series.expected_interval_hours,
                    trend: series.trend,
                    hours_since_last_signal: series.hours_since_last_signal,
                    stronger_tier_tracked: series.stronger_tier_tracked,
                })
                .collect())
        })
        .await
    }
}

#[async_trait]
impl AutopilotGrowthMetricRepository for PostgresAutopilotRepository {
    async fn upsert_growth_metric_series(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertGrowthMetricSeries,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<GrowthMetricSeriesMutation, RepositoryError> {
        self.bounded(async {
            if command.display_name.trim().is_empty()
                || command.metric_key.trim().is_empty()
                || command.expected_interval_hours == 0
                || command.expected_interval_hours > 720
            {
                return Err(RepositoryError::Unexpected);
            }
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;

            let subject_kind = command.subject.map(GrowthMetricSubject::kind);
            let subject_id = command.subject.map(GrowthMetricSubject::uuid);

            // Identity is `(platform, metric_key, subject)`. Looking it up first
            // means re-declaring a series updates the existing timeline instead
            // of opening a second one for the same number.
            let existing_id = sqlx::query_scalar::<_, Uuid>(
                r#"
                SELECT id
                FROM viryaos_growth_metric_series
                WHERE workspace_id = $1
                  AND platform = $2
                  AND metric_key = $3
                  AND subject_kind IS NOT DISTINCT FROM $4
                  AND subject_id IS NOT DISTINCT FROM $5
                FOR UPDATE
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.platform.as_str())
            .bind(&command.metric_key)
            .bind(subject_kind)
            .bind(subject_id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            let series_id = existing_id.unwrap_or_else(Uuid::now_v7);
            let operation_id = Uuid::now_v7();
            let details = json!({
                "platform": command.platform.as_str(),
                "metric_key": &command.metric_key,
                "subject_kind": subject_kind,
                "subject_id": subject_id,
                "display_name": &command.display_name,
                "direction": command.direction.as_str(),
                "value_tier": command.value_tier.as_str(),
                "expected_interval_hours": command.expected_interval_hours,
                "active": command.active,
            });
            if let Some(existing_operation_id) = insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "upsert_autopilot_growth_metric_series",
                "growth_metric_series",
                series_id,
                idempotency_key,
                request_id,
                &details,
            )
            .await?
            {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(GrowthMetricSeriesMutation {
                    operation_id: existing_operation_id,
                    series_id: GrowthMetricSeriesId::from_uuid(series_id),
                    replayed: true,
                });
            }

            let interval = i32::try_from(command.expected_interval_hours)
                .map_err(|_| RepositoryError::Unexpected)?;
            sqlx::query(
                r#"
                INSERT INTO viryaos_growth_metric_series (
                    id, workspace_id, platform, metric_key, subject_kind, subject_id,
                    display_name, direction, value_tier, expected_interval_hours, active
                ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
                ON CONFLICT (workspace_id, platform, metric_key, subject_kind, subject_id)
                DO UPDATE SET
                    display_name = EXCLUDED.display_name,
                    direction = EXCLUDED.direction,
                    value_tier = EXCLUDED.value_tier,
                    expected_interval_hours = EXCLUDED.expected_interval_hours,
                    active = EXCLUDED.active
                "#,
            )
            .bind(series_id)
            .bind(workspace_id.into_uuid())
            .bind(command.platform.as_str())
            .bind(&command.metric_key)
            .bind(subject_kind)
            .bind(subject_id)
            .bind(&command.display_name)
            .bind(command.direction.as_str())
            .bind(command.value_tier.as_str())
            .bind(interval)
            .bind(command.active)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            transaction.commit().await.map_err(map_sqlx)?;
            Ok(GrowthMetricSeriesMutation {
                operation_id,
                series_id: GrowthMetricSeriesId::from_uuid(series_id),
                replayed: false,
            })
        })
        .await
    }

    async fn record_growth_metric_point(
        &self,
        workspace_id: WorkspaceId,
        command: RecordGrowthMetricPoint,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<GrowthMetricPointMutation, RepositoryError> {
        self.bounded(async {
            if command.value < 0 || command.source.trim().is_empty() {
                return Err(RepositoryError::Unexpected);
            }
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;

            let series_exists = sqlx::query_scalar::<_, bool>(
                r#"
                SELECT EXISTS (
                    SELECT 1 FROM viryaos_growth_metric_series
                    WHERE workspace_id = $1 AND id = $2
                )
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.series_id.into_uuid())
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if !series_exists {
                return Err(RepositoryError::NotFound);
            }

            let operation_id = Uuid::now_v7();
            let details = json!({
                "series_id": command.series_id.into_uuid(),
                "captured_at": command.captured_at,
                "value": command.value,
                "source": &command.source,
            });
            if let Some(existing_operation_id) = insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "record_autopilot_growth_metric_point",
                "growth_metric_series",
                command.series_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?
            {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(GrowthMetricPointMutation {
                    operation_id: existing_operation_id,
                    series_id: command.series_id,
                    replayed: true,
                    accepted: false,
                });
            }

            // `DO NOTHING`, not `DO UPDATE`. A provider re-delivering an
            // observation must not silently rewrite a value that a decision has
            // already been derived from; correcting history is a separate,
            // deliberate operation.
            let inserted = sqlx::query_scalar::<_, i64>(
                r#"
                INSERT INTO viryaos_growth_metric_points (
                    workspace_id, series_id, captured_at, value, source
                ) VALUES ($1,$2,$3,$4,$5)
                ON CONFLICT (workspace_id, series_id, captured_at) DO NOTHING
                RETURNING id
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.series_id.into_uuid())
            .bind(command.captured_at)
            .bind(command.value)
            .bind(&command.source)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            transaction.commit().await.map_err(map_sqlx)?;
            Ok(GrowthMetricPointMutation {
                operation_id,
                series_id: command.series_id,
                replayed: false,
                accepted: inserted.is_some(),
            })
        })
        .await
    }

    async fn load_growth_metric_trends(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<GrowthMetricTrendView>, RepositoryError> {
        self.bounded(async {
            let policy = self.growth_metric_policy(workspace_id).await?;
            let loaded = self.load_series(workspace_id, now).await?;
            Ok(loaded
                .into_iter()
                .map(|series| {
                    let stale_after = i64::from(series.expected_interval_hours)
                        .saturating_mul(3_600)
                        .saturating_mul(i64::from(policy.stale_interval_basis_points))
                        .saturating_div(10_000);
                    GrowthMetricTrendView {
                        series_id: GrowthMetricSeriesId::from_uuid(series.row.id),
                        platform: series.platform,
                        metric_key: series.row.metric_key,
                        display_name: series.row.display_name,
                        subject_kind: series.row.subject_kind,
                        subject_id: series.row.subject_id,
                        direction: series.direction,
                        value_tier: series.value_tier,
                        expected_interval_hours: series.expected_interval_hours,
                        latest_value: series.trend.latest_value,
                        latest_at: series.trend.latest_at,
                        delta_24h: series.trend.delta_24h,
                        delta_7d: series.trend.delta_7d,
                        delta_28d: series.trend.delta_28d,
                        velocity_milli_per_day: series.trend.velocity_milli_per_day,
                        baseline_milli_per_day: series.trend.baseline_milli_per_day,
                        velocity_ratio_basis_points: velocity_ratio_basis_points(
                            series.trend,
                            series.direction,
                            policy.minimum_baseline_milli_per_day,
                        ),
                        points_in_window: series.trend.points_in_window,
                        age_seconds: series.trend.age_seconds,
                        stale: stale_after > 0 && series.trend.age_seconds > stale_after,
                    }
                })
                .collect())
        })
        .await
    }
}

/// First-party series CrowdRelay maintains itself. Declared here rather than in
/// the database so the shape of a system-owned series stays reviewable in one
/// place, and so an operator cannot silently retarget one by editing a row.
///
/// The cadence is deliberately looser than the write rate: points are recorded
/// hourly, but a six-hour expectation means an ordinary worker restart or a
/// short outage does not read as a dead feed.
const FIRST_PARTY_INTERVAL_HOURS: i32 = 6;

impl PostgresAutopilotRepository {
    /// Declares the per-event ticketing series for every event currently worth
    /// tracking, and retires the ones that have aged out.
    ///
    /// Retiring matters as much as declaring: a series for a show that finished
    /// months ago would stop receiving points and then be reported as a dead
    /// feed forever, which is noise dressed up as a finding.
    async fn sync_event_ticketing_series(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<(u32, u32), RepositoryError> {
        let workspace = workspace_id.into_uuid();
        let tracked = sqlx::query_scalar::<_, i64>(
            r#"
            WITH tracked AS (
                INSERT INTO viryaos_growth_metric_series (
                    workspace_id, platform, metric_key, subject_kind, subject_id,
                    display_name, direction, value_tier, expected_interval_hours, active
                )
                SELECT
                    event.workspace_id,
                    'ticketing',
                    metric.metric_key,
                    'event',
                    event.id,
                    left(metric.label || ' — ' || event.title, 120),
                    'higher_is_better',
                    'downstream',
                    $3,
                    true
                FROM events AS event
                CROSS JOIN (VALUES
                    ('paid_tickets', 'Paid tickets'),
                    ('paid_buyers', 'Paid buyers')
                ) AS metric(metric_key, label)
                WHERE event.workspace_id = $1
                  AND event.status IN ('published', 'completed')
                  AND event.starts_at >= $2 - INTERVAL '30 days'
                  AND event.starts_at <= $2 + INTERVAL '365 days'
                ON CONFLICT (workspace_id, platform, metric_key, subject_kind, subject_id)
                DO UPDATE SET
                    display_name = EXCLUDED.display_name,
                    active = true
                RETURNING 1
            )
            SELECT count(*)::bigint FROM tracked
            "#,
        )
        .bind(workspace)
        .bind(now)
        .bind(FIRST_PARTY_INTERVAL_HOURS)
        .fetch_one(&mut **transaction)
        .await
        .map_err(map_sqlx)?;

        let retired = sqlx::query_scalar::<_, i64>(
            r#"
            WITH retired AS (
                UPDATE viryaos_growth_metric_series AS series
                SET active = false
                WHERE series.workspace_id = $1
                  AND series.platform = 'ticketing'
                  AND series.subject_kind = 'event'
                  AND series.active
                  AND NOT EXISTS (
                      SELECT 1
                      FROM events AS event
                      WHERE event.workspace_id = series.workspace_id
                        AND event.id = series.subject_id
                        AND event.status IN ('published', 'completed')
                        AND event.starts_at >= $2 - INTERVAL '30 days'
                        AND event.starts_at <= $2 + INTERVAL '365 days'
                  )
                RETURNING 1
            )
            SELECT count(*)::bigint FROM retired
            "#,
        )
        .bind(workspace)
        .bind(now)
        .fetch_one(&mut **transaction)
        .await
        .map_err(map_sqlx)?;

        Ok((
            u32::try_from(tracked).unwrap_or(u32::MAX),
            u32::try_from(retired).unwrap_or(u32::MAX),
        ))
    }

    /// Declares the workspace-level series. These have no subject, which is why
    /// the series uniqueness has to treat NULL subjects as equal.
    async fn sync_workspace_series(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        workspace_id: WorkspaceId,
    ) -> Result<u32, RepositoryError> {
        let tracked = sqlx::query_scalar::<_, i64>(
            r#"
            WITH tracked AS (
                INSERT INTO viryaos_growth_metric_series (
                    workspace_id, platform, metric_key, subject_kind, subject_id,
                    display_name, direction, value_tier, expected_interval_hours, active
                )
                SELECT $1, metric.platform, metric.metric_key, NULL, NULL,
                       metric.label, 'higher_is_better', 'downstream', $2, true
                FROM (VALUES
                    ('signal', 'active_fans', 'Fans with an open account'),
                    ('signal', 'activated_fans_30d', 'Fans active in the last 30 days'),
                    ('merch', 'paid_orders', 'Paid merch orders')
                ) AS metric(platform, metric_key, label)
                ON CONFLICT (workspace_id, platform, metric_key, subject_kind, subject_id)
                DO UPDATE SET
                    display_name = EXCLUDED.display_name,
                    active = true
                RETURNING 1
            )
            SELECT count(*)::bigint FROM tracked
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(FIRST_PARTY_INTERVAL_HOURS)
        .fetch_one(&mut **transaction)
        .await
        .map_err(map_sqlx)?;
        Ok(u32::try_from(tracked).unwrap_or(u32::MAX))
    }
}

#[async_trait]
impl AutopilotFirstPartyGrowthMetrics for PostgresAutopilotRepository {
    async fn materialize_first_party_growth_metrics(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<FirstPartyGrowthMetricReport, RepositoryError> {
        self.bounded(async {
            let workspace = workspace_id.into_uuid();
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;

            let (event_series, series_retired) = self
                .sync_event_ticketing_series(&mut transaction, workspace_id, now)
                .await?;
            let workspace_series = self
                .sync_workspace_series(&mut transaction, workspace_id)
                .await?;

            // Observations are bucketed to the top of the hour and inserted with
            // DO NOTHING. The worker cycle is far shorter than an hour, so
            // without a bucket every cycle would append a near-duplicate point
            // and the window would describe our polling rate rather than the
            // business.
            let event_points = sqlx::query_scalar::<_, i64>(
                r#"
                WITH recorded AS (
                    INSERT INTO viryaos_growth_metric_points (
                        workspace_id, series_id, captured_at, value, source
                    )
                    SELECT series.workspace_id, series.id, date_trunc('hour', $2::timestamptz),
                           CASE series.metric_key
                               WHEN 'paid_tickets' THEN COALESCE(sold.paid_tickets, 0)
                               ELSE COALESCE(sold.paid_buyers, 0)
                           END,
                           'crowdrelay'
                    FROM viryaos_growth_metric_series AS series
                    JOIN events AS event
                      ON event.workspace_id = series.workspace_id
                     AND event.id = series.subject_id
                    LEFT JOIN ticket_sales AS sale
                      ON sale.workspace_id = event.workspace_id
                     AND sale.event_id = event.id
                     AND sale.active
                    LEFT JOIN LATERAL (
                        SELECT
                            COALESCE(SUM(item.quantity) FILTER (
                                WHERE orders.status IN ('paid', 'partially_refunded')
                            ), 0)::bigint AS paid_tickets,
                            COUNT(DISTINCT orders.buyer_email) FILTER (
                                WHERE orders.status IN ('paid', 'partially_refunded')
                            )::bigint AS paid_buyers
                        FROM ticket_orders AS orders
                        JOIN ticket_order_items AS item
                          ON item.workspace_id = orders.workspace_id
                         AND item.ticket_order_id = orders.id
                        WHERE orders.workspace_id = event.workspace_id
                          AND orders.ticket_sale_id = sale.id
                    ) AS sold ON true
                    WHERE series.workspace_id = $1
                      AND series.platform = 'ticketing'
                      AND series.subject_kind = 'event'
                      AND series.active
                    ON CONFLICT (workspace_id, series_id, captured_at) DO NOTHING
                    RETURNING 1
                )
                SELECT count(*)::bigint FROM recorded
                "#,
            )
            .bind(workspace)
            .bind(now)
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            let workspace_points = sqlx::query_scalar::<_, i64>(
                r#"
                WITH totals AS (
                    SELECT
                        (SELECT count(*)::bigint FROM fans
                          WHERE workspace_id = $1 AND status = 'active') AS active_fans,
                        -- The definition lives in crowdrelay_domain::fan_activation
                        -- and this is its set-oriented form: signed up, consented,
                        -- and at least one meaningful action inside the window.
                        -- An account whose status is 'active' is not a person who
                        -- did anything, which is what the other series counts and
                        -- why it is not the campaign KPI.
                        (SELECT count(*)::bigint FROM fans AS fan
                          WHERE fan.workspace_id = $1
                            AND fan.status = 'active'
                            AND EXISTS (
                                SELECT 1 FROM fan_consents AS consent
                                WHERE consent.workspace_id = fan.workspace_id
                                  AND consent.fan_id = fan.id
                                  AND consent.purpose = 'marketing'
                                  AND consent.granted
                                  AND consent.recorded_at = (
                                      SELECT max(latest.recorded_at) FROM fan_consents AS latest
                                      WHERE latest.workspace_id = fan.workspace_id
                                        AND latest.fan_id = fan.id
                                        AND latest.purpose = 'marketing'
                                  )
                            )
                            AND fan_last_meaningful_action(fan.workspace_id, fan.id, fan.normalized_email)
                                BETWEEN $2 - INTERVAL '30 days' AND $2
                        ) AS activated_fans_30d,
                        (SELECT count(*)::bigint FROM merch_order_facts
                          WHERE workspace_id = $1) AS paid_orders
                ), recorded AS (
                    INSERT INTO viryaos_growth_metric_points (
                        workspace_id, series_id, captured_at, value, source
                    )
                    SELECT series.workspace_id, series.id, date_trunc('hour', $2::timestamptz),
                           CASE series.metric_key
                               WHEN 'active_fans' THEN totals.active_fans
                               WHEN 'activated_fans_30d' THEN totals.activated_fans_30d
                               ELSE totals.paid_orders
                           END,
                           'crowdrelay'
                    FROM viryaos_growth_metric_series AS series
                    CROSS JOIN totals
                    WHERE series.workspace_id = $1
                      AND series.subject_kind IS NULL
                      AND series.active
                      AND (series.platform, series.metric_key) IN (
                          ('signal', 'active_fans'),
                          ('signal', 'activated_fans_30d'),
                          ('merch', 'paid_orders')
                      )
                    ON CONFLICT (workspace_id, series_id, captured_at) DO NOTHING
                    RETURNING 1
                )
                SELECT count(*)::bigint FROM recorded
                "#,
            )
            .bind(workspace)
            .bind(now)
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            transaction.commit().await.map_err(map_sqlx)?;
            Ok(FirstPartyGrowthMetricReport {
                series_tracked: event_series.saturating_add(workspace_series),
                series_retired,
                points_recorded: u32::try_from(event_points.saturating_add(workspace_points))
                    .unwrap_or(u32::MAX),
            })
        })
        .await
    }
}
