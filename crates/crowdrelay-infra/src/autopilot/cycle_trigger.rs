//! Operator-initiated autopilot cycles: request one, or preview what one would do.
//!
//! The request path deliberately does no work of its own. It sends a NOTIFY and
//! returns; the worker's existing loop wakes and runs the same `run_once` a
//! scheduled tick runs. That matters more than it looks: the 24-hour action
//! quota is enforced inside the transaction that writes an action, so routing
//! the manual run through the same loop means a button cannot outrun the
//! guardrails, and there is no second execution path to keep in step.
//!
//! The preview path is strictly read-only. It loads the same snapshots the
//! evaluator would and reports the strategy those imply, without dispatching
//! anything — so an operator can see what the brain currently believes before
//! letting it act.

use crowdrelay_application::RepositoryError;
use crowdrelay_brain::GrowthStrategy;
use crowdrelay_domain::WorkspaceId;
use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;

use super::map_sqlx;

/// Channel the autopilot worker listens on. Must match
/// `crowdrelay_worker::autopilot::AUTOPILOT_CYCLE_CHANNEL`; the pair is pinned
/// by `scripts/test_autopilot_manual_cycle_v1.py`.
pub const AUTOPILOT_CYCLE_CHANNEL: &str = "autopilot_cycle";

/// What the brain currently believes, and what it would do about it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CyclePreview {
    /// Strategy the current world model implies.
    pub strategy: String,
    /// Template order that strategy prioritizes.
    pub template_priority: Vec<String>,
    /// Templates the brain has snapshots for this cycle.
    pub templates_considered: usize,
    /// Fans held today, first-party.
    pub total_fans: u32,
    /// Reachable audience summed across connected off-platform feeds.
    pub off_platform_audience: u32,
    /// Growth in that audience since the start of this month.
    pub off_platform_audience_this_month: u32,
    /// Feeds with any observation.
    pub connected_platforms: u32,
    /// Feeds whose newest observation is recent enough to act on. A gap from
    /// `connected_platforms` is measurement debt, not audience loss.
    pub fresh_platforms: u32,
    /// Metric the tenant optimizes.
    pub north_star: String,
    /// Current level of that metric.
    pub north_star_current: u32,
    /// Its growth this month.
    pub north_star_this_month: u32,
    /// Whether anything is connected at all. False means a cycle would run and
    /// correctly decide to do nothing, which is the honest answer rather than a
    /// failure.
    pub has_any_connected_platform: bool,
}

/// Asks the worker to run a cycle now.
///
/// Returns `Ok(())` once the notification is committed. Delivery is
/// at-most-once by nature — `NOTIFY` reaches only currently-listening sessions
/// — which is the right semantic here: if no worker is listening there is
/// nothing to run, and the next scheduled tick covers it anyway.
pub async fn request_autopilot_cycle(
    pool: &PgPool,
    workspace_id: WorkspaceId,
) -> Result<(), RepositoryError> {
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(AUTOPILOT_CYCLE_CHANNEL)
        .bind(workspace_id.into_uuid().to_string())
        .execute(pool)
        .await
        .map_err(map_sqlx)?;
    Ok(())
}

/// Reports what a cycle would decide, without running one.
pub async fn preview_autopilot_cycle(
    repo: &crate::autopilot::PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<CyclePreview, RepositoryError> {
    use crowdrelay_application::autopilot::AutopilotDecisionRepository;

    let snapshots = repo
        .load_growth_intelligence_snapshots(workspace_id, now)
        .await?;
    let Some(first) = snapshots.first() else {
        return Err(RepositoryError::Unexpected);
    };
    let world = &first.world_model;
    let strategy = GrowthStrategy::from_world_model(world);

    Ok(CyclePreview {
        strategy: strategy.as_str().to_owned(),
        template_priority: strategy
            .template_priority()
            .iter()
            .map(|template| (*template).to_owned())
            .collect(),
        templates_considered: snapshots.len(),
        total_fans: world.total_fans,
        off_platform_audience: world.off_platform_audience,
        off_platform_audience_this_month: world.off_platform_audience_this_month,
        connected_platforms: world.connected_platforms,
        fresh_platforms: world.fresh_platforms,
        north_star: world.north_star.as_str().to_owned(),
        north_star_current: world.north_star_current,
        north_star_this_month: world.north_star_this_month,
        has_any_connected_platform: world.connected_platforms > 0,
    })
}
