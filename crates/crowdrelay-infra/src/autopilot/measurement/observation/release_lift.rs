use super::*;
use crowdrelay_application::autopilot::{AutopilotMeasurementObservation, AutopilotSeriesLift};
use crowdrelay_domain::growth_metrics::MetricDirection;

/// Preserve platform/metric identity. Every complete series contributes one
/// original-unit contrast; a missing baseline is not an observed zero.
pub(super) async fn observe(
    pool: &sqlx::PgPool,
    workspace: WorkspaceId,
    measurement: &ClaimedAutopilotMeasurement,
    now: OffsetDateTime,
) -> Result<AutopilotMeasurementObservation, RepositoryError> {
    let rows = sqlx::query_as::<_, (uuid::Uuid, String, String, String, f64)>(
        r#"
        SELECT id, platform, metric_key, direction,
               (post_end::double precision - 2.0 * pre_end::double precision
                + pre_start::double precision)::double precision AS lift
        FROM (
            SELECT s.id, s.platform, s.metric_key, s.direction,
                (SELECT p.value FROM growth_metric_points p
                 WHERE p.workspace_id = $1 AND p.series_id = s.id
                   AND p.captured_at >= $3::timestamptz - INTERVAL '28 days'
                   AND p.captured_at < $3::timestamptz - INTERVAL '14 days'
                 ORDER BY p.captured_at DESC LIMIT 1) AS pre_start,
                (SELECT p.value FROM growth_metric_points p
                 WHERE p.workspace_id = $1 AND p.series_id = s.id
                   AND p.captured_at >= $3::timestamptz - INTERVAL '14 days'
                   AND p.captured_at < $3
                 ORDER BY p.captured_at DESC LIMIT 1) AS pre_end,
                (SELECT p.value FROM growth_metric_points p
                 WHERE p.workspace_id = $1 AND p.series_id = s.id
                   AND p.captured_at >= $3::timestamptz + INTERVAL '10 days'
                   AND p.captured_at < $3::timestamptz + INTERVAL '14 days'
                   AND p.captured_at <= $4
                 ORDER BY p.captured_at DESC LIMIT 1) AS post_end
            FROM growth_metric_series s
            WHERE s.workspace_id = $1 AND s.subject_kind = 'release_plan'
              AND s.subject_id = $2 AND s.active
        ) AS series
        WHERE pre_start IS NOT NULL AND pre_end IS NOT NULL AND post_end IS NOT NULL
        ORDER BY platform, metric_key, id
        "#,
    )
    .bind(workspace.into_uuid())
    .bind(measurement.subject_id)
    .bind(measurement.action_finished_at)
    .bind(now)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;
    if rows.is_empty() {
        return Err(RepositoryError::ConflictBecause(
            AutopilotMeasurementKind::NO_RELEASE_SERIES_DATA,
        ));
    }
    let mut series_lifts = Vec::with_capacity(rows.len());
    for (series_id, platform, metric_key, direction, lift) in rows {
        series_lifts.push(AutopilotSeriesLift {
            series_id,
            platform,
            metric_key,
            direction: MetricDirection::parse(&direction).ok_or(RepositoryError::Unexpected)?,
            lift,
        });
    }
    let observation = AutopilotMeasurementObservation {
        // Compatibility readout only; a mixed aggregate never teaches or
        // classifies growth. Its dimensioned facts are persisted alongside it.
        value: series_lifts.iter().map(|series| series.lift).sum(),
        series_lifts,
    };
    if !observation.is_valid_for(measurement.kind) {
        return Err(RepositoryError::Unexpected);
    }
    Ok(observation)
}
