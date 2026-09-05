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
use crowdrelay_brain::{GrowthStrategy, self_assessment::DailyNorthStar};
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
    /// Every connected feed has stopped reporting. The brain will not plan more
    /// discovery through them, which is why the strategy above may not be the
    /// one the growth numbers alone would suggest — and it is the single fact
    /// an operator needs in order to go and fix a credential.
    pub discovery_channels_silent: bool,
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

/// How a cycle was started. A brain that only ever runs when asked is a
/// different problem from one that runs on schedule and decides nothing.
#[derive(Clone, Copy, Debug)]
pub enum CycleTrigger {
    Scheduled,
    Requested,
}

impl CycleTrigger {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Requested => "requested",
        }
    }
}

/// Opens a cycle-run row and returns its id.
///
/// A cycle runs four isolated phases, each logging its own line, and until this
/// existed nothing tied them together: `phase_failed` collapsed every phase into
/// one boolean and the only evidence a cycle had happened was a scatter of log
/// lines with no shared identifier. Answering "which cycle produced that
/// decision, and what else did it do" meant correlating timestamps across four
/// tables and a log.
///
/// Returns `None` when the row cannot be written. The caller runs the cycle
/// regardless: this is an operator's record of what the brain did, and losing
/// the record must never cost the work it describes.
pub async fn open_cycle_run(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    trigger: CycleTrigger,
    now: OffsetDateTime,
) -> Option<uuid::Uuid> {
    let id = uuid::Uuid::now_v7();
    let written = sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_cycle_runs (id, workspace_id, trigger, started_at)
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(id)
    .bind(workspace_id.into_uuid())
    .bind(trigger.as_str())
    .bind(now)
    .execute(pool)
    .await;
    match written {
        Ok(_) => Some(id),
        Err(error) => {
            tracing::warn!(error = %error, "could not open an autopilot cycle run record");
            None
        }
    }
}

/// Closes a cycle-run row, counting what the cycle produced.
///
/// The counts are read back from the tables that hold the truth rather than
/// accumulated as the cycle runs, so this record cannot drift away from the
/// ledger it describes. `degraded` rather than `failed` when a phase fell over:
/// the phases are isolated on purpose, so one failing while the others complete
/// is the design working, and calling that a failed cycle would train the
/// operator to ignore the word.
/// `north_star_observed` is the reading the cycle itself took, not a figure
/// derived here. This used to be a hardcoded count of active fans, which is not
/// the North Star for any tenant that has not chosen fans — and the default is
/// signal installs. The result was two numbers under one name: the world model's
/// on `/autopilot/cycle/preview`, this one on `/ops/cycles`, and the brain's
/// self-assessment trending whichever of them the brain was not optimizing. The
/// two populations did not even agree on what a fan is: the world model counts
/// every fan that is not `suppressed`, this counted only `active`.
pub async fn close_cycle_run(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    cycle_id: uuid::Uuid,
    degraded: bool,
    finished_at: OffsetDateTime,
    north_star_observed: Option<u32>,
) {
    let closed = sqlx::query(
        r#"
        UPDATE viryaos_autopilot_cycle_runs AS run
        SET finished_at = $3,
            duration_ms = GREATEST(0, (EXTRACT(EPOCH FROM ($3 - run.started_at)) * 1000)::integer),
            outcome = CASE WHEN $4 THEN 'degraded' ELSE 'succeeded' END,
            decisions_recorded = (
                SELECT count(*)
                FROM viryaos_autopilot_decisions AS decision
                WHERE decision.workspace_id = run.workspace_id
                  AND decision.evaluated_at >= run.started_at
                  AND decision.evaluated_at <= $3
            ),
            actions_created = (
                SELECT count(*)
                FROM viryaos_autopilot_actions AS action
                WHERE action.workspace_id = run.workspace_id
                  AND action.created_at >= run.started_at
                  AND action.created_at <= $3
            ),
            -- The reading the cycle took, so the brain's assessment of itself is
            -- measured against the metric the brain is actually optimizing. A
            -- cycle whose evaluation phase never ran records NULL rather than a
            -- zero, which would be indistinguishable from losing the audience.
            north_star_value = $5
        WHERE run.workspace_id = $1 AND run.id = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(cycle_id)
    .bind(finished_at)
    .bind(degraded)
    .bind(north_star_observed.and_then(|value| i32::try_from(value).ok()))
    .execute(pool)
    .await;
    if let Err(error) = closed {
        tracing::warn!(error = %error, "could not close an autopilot cycle run record");
    }
}

/// How far back `daily_north_star` looks.
///
/// `STAGNATION_AFTER_FLAT_DAYS` is 30, and a day on which no reading could be
/// taken -- the worker was down, the cycle never closed -- contributes nothing.
/// Sixty days of window leaves room for those gaps while still reaching thirty
/// samples, so stagnation stays reachable rather than being defeated by a
/// single missing day.
pub const NORTH_STAR_WINDOW_DAYS: i32 = 60;

/// One North Star reading per day, newest day first.
///
/// Exists because the assessment must not be computed from the cycle list an
/// operator happens to be looking at. That list is capped at 200 rows and
/// defaults to 20, and a cycle runs every five minutes by default, so the page
/// spans at most sixteen hours: one or two distinct days against the six
/// `self_assessment::assess` needs before it will claim a trend at all. Reading
/// the trend off the page made `brain_state` permanently `initializing` --
/// which reads as honest reticence and is really a window that can never be
/// filled. The list is also filterable by outcome, so `?state=degraded` would
/// have silently changed the reported health of the brain.
///
/// `DISTINCT ON` keeps the last reading of each day, because the fan count is
/// cumulative and the end of the day is the day's result. The predicate matches
/// `autopilot_cycle_runs_north_star_idx`.
pub async fn daily_north_star(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    window_days: i32,
) -> Result<Vec<DailyNorthStar>, RepositoryError> {
    let rows = sqlx::query_as::<_, (time::Date, i32)>(
        r#"
        SELECT DISTINCT ON (started_at::date)
               started_at::date AS day,
               north_star_value
        FROM viryaos_autopilot_cycle_runs
        WHERE workspace_id = $1
          AND north_star_value IS NOT NULL
          AND started_at >= now() - ($2::int * interval '1 day')
        ORDER BY started_at::date DESC, started_at DESC
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(window_days)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    Ok(rows
        .into_iter()
        .map(|(day, value)| DailyNorthStar {
            day: i64::from(day.to_julian_day()),
            value: f64::from(value),
        })
        .collect())
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
        // The ranked order, so the preview shows the order the cycle will
        // actually use rather than the list as written.
        template_priority: strategy
            .template_priority_for(world)
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
        discovery_channels_silent: GrowthStrategy::discovery_channels_are_silent(world),
    })
}
