//! Persistence for the fan-source attribution the brain already computes.
//!
//! `attribute_fan_growth` runs inside `load_causal_model` and its per-template
//! and per-strategy breakdowns used to die in a log line; the same happened to
//! `detect_fan_growth_shifts` over the daily North Star series. The worker
//! snapshots both once per cycle here, so the operator's "where did our fans
//! come from" and "when did the rate change" survive longer than a container
//! log — and arrive in one read, off the same row.
//!
//! One row per workspace per hour: the WHERE NOT EXISTS guard is the cap, in
//! the database rather than in worker state, so a manual "run a cycle now"
//! and a scheduled tick cannot double-write the hour — they take the same
//! `run_once` path and race the same guard.

use super::map_sqlx;
use crowdrelay_application::RepositoryError;
use crowdrelay_domain::WorkspaceId;
use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;

/// The snapshot cadence the operator-facing table is shaped around.
const SNAPSHOT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// One detected North Star regime shift, with its civil date resolved.
///
/// `ChangePoint.timestamp` is a zero-based observation index despite its
/// name — a UI that rendered it as a date would display nonsense, so the
/// resolved date is stored instead and the index is dropped. `pre_mean` and
/// `post_mean` are the rates on each side of the shift in the series' own
/// units (fans per day); `shift_size` is their distance.
#[derive(Debug, Serialize)]
pub struct NorthStarShift {
    /// The civil date of the observation the detector fired on — the first
    /// day of the new regime, not the day the old one ended.
    pub date: String,
    /// `"upward"` or `"downward"` — in real time, which is why the series
    /// must reach the detector oldest-first.
    pub direction: String,
    pub pre_mean: f64,
    pub post_mean: f64,
    pub shift_size: f64,
    /// The CUSUM statistic that crossed the threshold — how sustained the
    /// shift was, not how large.
    pub magnitude: f64,
}

/// Maps detected change points onto the days they fired on.
///
/// `days` must be the same series the detector saw, in the same order —
/// oldest first — or the index resolves to the wrong date. An index past the
/// end of the series is skipped rather than wrapped: a shift without a date
/// would store a worse answer than no shift.
#[must_use]
pub fn resolve_shifts(
    days: &[crowdrelay_brain::self_assessment::DailyNorthStar],
    points: &[crowdrelay_brain::change_point::ChangePoint],
) -> Vec<NorthStarShift> {
    points
        .iter()
        .filter_map(|point| {
            let julian = i32::try_from(days.get(point.timestamp)?.day).ok()?;
            let date = time::Date::from_julian_day(julian).ok()?;
            Some(NorthStarShift {
                date: date.to_string(),
                direction: point.direction.as_str().to_owned(),
                pre_mean: point.pre_mean,
                post_mean: point.post_mean,
                shift_size: point.shift_size(),
                magnitude: point.magnitude,
            })
        })
        .collect()
}

/// Whether a snapshot already covers this hour — the cheap pre-check the
/// worker runs before paying for a full evidence replay it would discard.
/// The real cap stays the INSERT's own guard; this only skips the work.
pub async fn fan_source_snapshot_due(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<bool, RepositoryError> {
    let covered = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM fan_source_snapshots
            WHERE workspace_id = $1
              AND captured_at >= $2
        )
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now - SNAPSHOT_INTERVAL)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(!covered)
}

/// Writes one attribution snapshot for the workspace unless one was captured
/// within the last hour. Returns whether a row was actually written.
///
/// `captured_at` is taken from the caller, not `now()`, so the row carries the
/// cycle's own clock — the timestamp the decision record uses — rather than a
/// second reading taken a few milliseconds later.
pub async fn record_fan_source_snapshot(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    attribution: &crowdrelay_brain::attribution::FanGrowthAttribution,
    north_star_shifts: &[NorthStarShift],
    captured_at: OffsetDateTime,
) -> Result<bool, RepositoryError> {
    let payload = serde_json::to_value(attribution).map_err(|error| {
        tracing::warn!(error = %error, "fan-growth attribution failed to serialize");
        RepositoryError::Unexpected
    })?;
    let shifts = serde_json::to_value(north_star_shifts).map_err(|error| {
        tracing::warn!(error = %error, "north star shifts failed to serialize");
        RepositoryError::Unexpected
    })?;
    let written = sqlx::query_scalar::<_, bool>(
        r#"
        INSERT INTO fan_source_snapshots (
            workspace_id, captured_at, attribution, north_star_shifts,
            total_observed_fans, total_incremental_fans,
            total_durable_fans, resolved_observations
        )
        SELECT $1, $2, $3, $4, $5, $6, $7, $8
        WHERE NOT EXISTS (
            SELECT 1
            FROM fan_source_snapshots
            WHERE workspace_id = $1
              AND captured_at >= $9
        )
        RETURNING true
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(captured_at)
    .bind(payload)
    .bind(shifts)
    .bind(attribution.total_observed_fans)
    .bind(attribution.total_incremental_fans)
    .bind(attribution.total_durable_fans)
    .bind(i32::try_from(attribution.resolved_observations).unwrap_or(i32::MAX))
    .bind(captured_at - SNAPSHOT_INTERVAL)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(written.unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crowdrelay_brain::change_point::ChangeDirection;
    use crowdrelay_brain::self_assessment::DailyNorthStar;

    fn day(date: time::Date, value: f64) -> DailyNorthStar {
        DailyNorthStar {
            day: i64::from(date.to_julian_day()),
            value,
        }
    }

    #[test]
    fn resolves_shift_index_to_the_civil_date_it_fired_on() {
        // Thirty flat days then a clean step — the detector must fire, and
        // the stored date must be the day the firing observation landed, not
        // the index and not the day before.
        let first = time::macros::date!(2026 - 08 - 01);
        let days: Vec<DailyNorthStar> = (0..40)
            .map(|i| {
                let date = first + time::Duration::days(i);
                day(date, if i < 30 { 1.0 } else { 20.0 })
            })
            .collect();
        let series: Vec<f64> = days.iter().map(|d| d.value).collect();
        let points = crowdrelay_brain::change_point::detect_fan_growth_shifts(&series, 10.0, 2.0);
        assert!(!points.is_empty(), "a 19-fan step must fire the detector");

        let shifts = resolve_shifts(&days, &points);
        assert_eq!(shifts.len(), points.len());
        let first_shift = &shifts[0];
        // The date is a civil date of a real seeded day — never an index.
        let expected = (first + time::Duration::days(points[0].timestamp as i64)).to_string();
        assert_eq!(first_shift.date, expected);
        assert!(
            first_shift.date.starts_with("2026-"),
            "stored date must be ISO, got {}",
            first_shift.date
        );
        assert_eq!(first_shift.direction, "upward");
        assert!(first_shift.shift_size > 0.0);
    }

    #[test]
    fn reversed_series_would_report_the_same_step_as_downward() {
        // The detector documents oldest-first input. This pins why: fed
        // newest-first, a real upward step reports as a downward shift —
        // the call-site bug this feature's log line had.
        let days: Vec<f64> = (0..40).map(|i| if i < 30 { 1.0 } else { 20.0 }).collect();
        let reversed: Vec<f64> = days.iter().rev().copied().collect();
        let points = crowdrelay_brain::change_point::detect_fan_growth_shifts(&reversed, 10.0, 2.0);
        assert_eq!(points[0].direction, ChangeDirection::Downward);
    }

    #[test]
    fn index_past_the_series_is_skipped_not_wrapped() {
        let days = vec![day(time::macros::date!(2026 - 09 - 01), 1.0)];
        let points = vec![crowdrelay_brain::change_point::ChangePoint {
            timestamp: 7,
            direction: ChangeDirection::Upward,
            magnitude: 12.0,
            pre_mean: 1.0,
            post_mean: 5.0,
        }];
        assert!(resolve_shifts(&days, &points).is_empty());
    }
}
