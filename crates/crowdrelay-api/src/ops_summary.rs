//! Compact serialization types and first-party watchdog visibility for operations.

use serde::Serialize;
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Default, FromRow, Serialize)]
pub(crate) struct QueueSummary {
    pub(crate) pending: i64,
    pub(crate) processing: i64,
    pub(crate) delivered_24h: i64,
    pub(crate) dead: i64,
    pub(crate) cancelled: i64,
    pub(crate) oldest_pending_seconds: i64,
}

#[derive(Debug, Default, FromRow, Serialize)]
pub(crate) struct WatchdogSummary {
    active_alerts: i64,
    critical_alerts: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    last_observed_at: Option<OffsetDateTime>,
}

pub(crate) async fn load_watchdog_summary(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<WatchdogSummary, sqlx::Error> {
    sqlx::query_as::<_, WatchdogSummary>(
        r#"
        SELECT
            count(*) FILTER (WHERE active)::bigint AS active_alerts,
            count(*) FILTER (WHERE active AND severity = 'critical')::bigint AS critical_alerts,
            max(last_seen_at) AS last_observed_at
        FROM viryaos_ops_alert_state
        WHERE workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await
}

/// Whether the worker process is alive, and for how long it has not been.
///
/// The worker serves no HTTP, so nothing could ask it directly. It renews its
/// leadership lease every 15 seconds, which makes the lease age the one honest
/// heartbeat — and this summary is already on the operator's screen.
///
/// This exists because the worker was killed by a deploy and stayed dead for
/// over fifteen minutes while every dashboard showed green.
///
/// `cycle_age_seconds` and `crash_looping` exist because a worker can keep
/// restarting and renewing its lease while never completing an autopilot
/// cycle — a crash-loop that looks alive on the lease but is functionally
/// dead. This happened for 7+ hours while the dashboard said "healthy".
#[derive(Debug, Serialize)]
pub(crate) struct WorkerSummary {
    /// Seconds since the last lease renewal. Renewal is every 15s.
    pub(crate) lease_age_seconds: i64,
    /// False once the lease is stale enough that the process cannot be running.
    pub(crate) alive: bool,
    /// Seconds since an autopilot cycle last *finished*. 999999 if none ever has.
    pub(crate) cycle_age_seconds: i64,
    /// Seconds since an autopilot decision was last evaluated. 999999 if none
    /// ever has.
    ///
    /// Reported next to `cycle_age_seconds` rather than instead of it, because
    /// the two answer different questions and only one of them is a fault. A
    /// long decision age on a healthy cycle means the brain is finding nothing
    /// worth doing, which is a growth-loop observation; a long cycle age means
    /// the worker is not getting through a cycle at all.
    pub(crate) decision_age_seconds: i64,
    /// True when the lease is fresh but no autopilot cycle has completed in
    /// [`WORKER_CYCLE_STALE_AFTER_SECONDS`]. This is the crash-loop signal:
    /// the worker keeps restarting and acquiring leadership but never gets
    /// far enough to finish a cycle.
    pub(crate) crash_looping: bool,
}

/// Twice the renewal interval plus the lease term: unambiguous death, not a
/// slow cycle.
const WORKER_LEASE_DEAD_AFTER_SECONDS: i64 = 120;

/// If the lease is fresh but no autopilot cycle has completed in this many
/// seconds, the worker is crash-looping. 30 minutes is generous: a normal
/// cycle runs every ~60s, and even a slow cycle with heavy provider calls
/// finishes in under 5 minutes.
const WORKER_CYCLE_STALE_AFTER_SECONDS: i64 = 1800;

pub(crate) async fn load_worker_summary(pool: &PgPool) -> Result<WorkerSummary, sqlx::Error> {
    // Not workspace-scoped: leadership is per deployment. A missing row means
    // no worker has ever run, which reads as dead rather than as healthy.
    let lease_age_seconds: i64 = sqlx::query_scalar(
        r#"
        SELECT COALESCE((
            SELECT EXTRACT(EPOCH FROM (
                now() - (expires_at - INTERVAL '60 seconds')
            ))::bigint
            FROM worker_leadership WHERE id = 1
        ), 999999)
        "#,
    )
    .fetch_one(pool)
    .await?;

    // Last *finished* cycle — the honest signal that the worker gets through a
    // cycle, not just that it acquired a lease.
    //
    // This used to read `MAX(evaluated_at)` from `viryaos_autopilot_decisions`,
    // which is a proxy and not the thing: a cycle that runs correctly and finds
    // nothing worth deciding writes no decision row. Measured in production
    // 2026-09-13, that reported `crash_looping: true` against a worker with
    // `RestartCount=0` and eight consecutive `succeeded` cycles, each finishing
    // in 300-1700ms. Thirty minutes of healthy empty cycles was enough to raise
    // a crash-loop alarm in the operator's first view.
    //
    // The proxy predates `viryaos_autopilot_cycle_runs` (migration 0233), which
    // records cycle completion directly and whose own comment names the case
    // this signal wants: "NULL means the cycle never finished: the process died
    // mid-cycle, which is otherwise indistinguishable from a cycle that ran and
    // decided nothing." Reading `finished_at` gets both halves right — it goes
    // stale when cycles stop finishing, and it does not when they finish empty.
    //
    // Both queries are deliberately cross-workspace: the worker serves all
    // workspaces and the operator needs to know whether ANY cycle has
    // completed recently, not whether one workspace has. Scoping them to a
    // single workspace would hide a stalled worker behind a workspace that
    // happens to have a recent cycle. The workspace-scope ratchet baseline
    // allows these unscoped statements for that reason.
    // One statement, two subselects: this view is on the operator's screen and
    // does not need a second round trip to answer a second question about the
    // same worker.
    let (cycle_age_seconds, decision_age_seconds): (i64, i64) = sqlx::query_as(
        r#"
        SELECT
            COALESCE((
                SELECT EXTRACT(EPOCH FROM (now() - MAX(finished_at)))::bigint
                FROM viryaos_autopilot_cycle_runs
            ), 999999),
            COALESCE((
                SELECT EXTRACT(EPOCH FROM (now() - MAX(evaluated_at)))::bigint
                FROM viryaos_autopilot_decisions
            ), 999999)
        "#,
    )
    .fetch_one(pool)
    .await?;

    let alive = lease_age_seconds <= WORKER_LEASE_DEAD_AFTER_SECONDS;
    let crash_looping = alive && cycle_age_seconds > WORKER_CYCLE_STALE_AFTER_SECONDS;

    Ok(WorkerSummary {
        lease_age_seconds,
        alive,
        cycle_age_seconds,
        decision_age_seconds,
        crash_looping,
    })
}
