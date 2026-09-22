//! The fan-source attribution snapshot — the cumulative "where did our fans
//! come from" ledger the operator sees.
//!
//! Runs once per cycle at the end of `run_once` so the measurement phases
//! land in the reading. Same evidence port and same pure functions the
//! evaluator's model load uses — a second invocation of the existing
//! calculations, not a second definition of them. Writes at most once an
//! hour; the guard lives in the INSERT itself, so a manually requested cycle
//! cannot outrun it either. Deliberately not inside `load_causal_model`:
//! that loader also serves the preview endpoint, and a read path must not
//! emit writes at a cadence nobody chose.

use crowdrelay_application::autopilot::AutopilotDecisionRepository;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::autopilot::{
    NORTH_STAR_WINDOW_DAYS, PostgresAutopilotRepository, daily_north_star, fan_source_snapshot_due,
    record_fan_source_snapshot, resolve_shifts,
};
use time::OffsetDateTime;

/// Runs the phase. Returns `false` when the phase must count as degraded —
/// the caller marks it, since `DegradedPhases` is cycle bookkeeping, not
/// this module's. A not-due hour or an unreadable North Star series is not
/// a degradation: the first skips the replay it would discard anyway, the
/// second still writes the attribution half of the snapshot.
pub(crate) async fn run(
    repository: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> bool {
    // The due-check runs first: the evidence load is a full-window replay
    // and there is no reason to pay for it eleven cycles out of twelve
    // when the hour is already written.
    match fan_source_snapshot_due(repository.pool(), workspace_id, now).await {
        Ok(true) => {}
        Ok(false) => return true,
        Err(error) => {
            tracing::warn!(error = %error, "CrowdRelay fan-source snapshot due-check failed");
            return false;
        }
    }
    let evidence = match repository.load_growth_evidence(workspace_id, None).await {
        Ok(evidence) => evidence,
        Err(error) => {
            tracing::warn!(error = %error, "CrowdRelay fan-source snapshot evidence load failed");
            return false;
        }
    };
    let attribution = crowdrelay_brain::attribution::attribute_fan_growth(&evidence);
    // The same series the evaluator's loader runs CUSUM over — but reversed
    // to oldest-first, which is the order the detector's contract requires.
    // `daily_north_star` returns newest-first; fed as-is it reports an
    // upward step as a downward shift.
    let shifts = match daily_north_star(repository.pool(), workspace_id, NORTH_STAR_WINDOW_DAYS)
        .await
    {
        Ok(mut days) => {
            days.reverse();
            let series: Vec<f64> = days.iter().map(|day| day.value).collect();
            let points =
                crowdrelay_brain::change_point::detect_fan_growth_shifts(&series, 10.0, 2.0);
            resolve_shifts(&days, &points)
        }
        Err(error) => {
            tracing::warn!(error = %error, "CrowdRelay north-star series load failed; snapshot stores no shifts");
            Vec::new()
        }
    };
    let shift_count = shifts.len();
    match record_fan_source_snapshot(repository.pool(), workspace_id, &attribution, &shifts, now)
        .await
    {
        Ok(written) => {
            if written {
                tracing::info!(
                    observed_fans = attribution.total_observed_fans,
                    incremental_fans = attribution.total_incremental_fans,
                    durable_fans = attribution.total_durable_fans,
                    resolved_observations = attribution.resolved_observations,
                    templates = attribution.by_template.len(),
                    strategies = attribution.by_strategy.len(),
                    north_star_shifts = shift_count,
                    "CrowdRelay recorded fan-source attribution snapshot"
                );
            }
            true
        }
        Err(error) => {
            tracing::warn!(error = %error, "CrowdRelay fan-source snapshot write failed");
            false
        }
    }
}
