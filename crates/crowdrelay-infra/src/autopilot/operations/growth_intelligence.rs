//! Snapshot loader for the deterministic growth intelligence brain.
//!
//! Returns one snapshot per worker template that the brain may dispatch.
//! Each snapshot carries the hours since the last run and the workspace's
//! current situation (upcoming events, fan growth, unengaged targets).
//! The deterministic evaluator consumes these to decide whether to dispatch.
//!
//! # Architecture
//!
//! The brain is a closed-loop learning system with five layers:
//!
//! 1. **World Model** — the brain's belief about the world: fan counts,
//!    signal installs, community reach, outreach pipeline, event state,
//!    and growth target progress. Loaded once per cycle from real data.
//! 2. **Causal Model** — P(new_fan | template, context) with hierarchical
//!    Gamma-Poisson (Negative Binomial) learning plus Normal-Normal
//!    treatment-effect posteriors. The brain predicts before dispatch and
//!    learns from prediction error after measurement (the dopamine loop).
//! 3. **Opportunity Queue + EFE** — each eligible dispatch is scored by
//!    Expected Free Energy, balancing pragmatic value (expected fans)
//!    against epistemic value (information gain). Lower EFE = better.
//! 4. **Exploration Memory** — tracks which (template, context) pairs have
//!    been explored, so the brain prefers novel territory (Go-Explore).
//! 5. **Hierarchical Planning** — a `GrowthStrategy` derived from the world
//!    model determines template priority order. Strategy → priority → EFE.
//!
//! All five layers are deterministic Rust. LLMs are workers that gather
//! intelligence and draft content — the brain decides strategy.

use super::*;

mod community_targets;
mod evidence_replay;
use evidence_replay::{
    PosteriorReplay, apply_evidence_to_model, apply_evidence_to_model_with_contrast,
    apply_evidence_to_stored_strategy_posterior,
};
#[cfg(test)]
mod learning_tests;
mod worker_signals;
use crowdrelay_application::autopilot::{BeliefStateOrigin, LoadedCausalModel};
use crowdrelay_brain::{
    CommunityEngagementSummary, GrowthIntelligenceSnapshot, GrowthTarget, GrowthTargetProgress,
    GrowthTrend, RecentInsight, TenantPreferencePosterior, WorldModel, agent_standing_policy,
    platform_yield::PlatformGrowth,
};
use crowdrelay_domain::growth_metrics::{MetricPlatform, NorthStarMetric};
use crowdrelay_domain::learning::{OutcomeRecord, Standing, assess_standing};
use crowdrelay_domain::worker_template::WorkerTemplate;

/// The month-over-month audience arithmetic, shared by the total and the
/// per-platform breakdown.
///
/// Both queries need the same four things: the series a caller asked for, the
/// latest reading of each, the reading it started the month at, and the
/// difference. Writing that twice meant two copies of a subtle
/// `DISTINCT ON`/baseline dance that have to stay identical to be comparable —
/// the per-platform numbers must add up to the total, and they only do if the
/// month boundary is drawn the same way in both.
///
/// `$1` workspace, `$2` platforms, `$3` metric keys.
const AUDIENCE_WINDOW_CTE: &str = r#"
        WITH wanted AS (
            SELECT * FROM unnest($2::text[], $3::text[]) AS t(platform, metric_key)
        ),
        target_series AS (
            SELECT s.id, s.platform
            FROM viryaos_growth_metric_series s
            JOIN wanted w ON w.platform = s.platform AND w.metric_key = s.metric_key
            WHERE s.workspace_id = $1 AND s.active
        ),
        points AS (
            SELECT p.series_id, s.platform, p.captured_at, p.value
            FROM viryaos_growth_metric_points p
            JOIN target_series s ON s.id = p.series_id
            WHERE p.workspace_id = $1
        ),
        latest AS (
            SELECT DISTINCT ON (series_id) series_id, platform, captured_at, value
            FROM points ORDER BY series_id, captured_at DESC
        ),
        before_month AS (
            SELECT DISTINCT ON (series_id) series_id, value
            FROM points
            WHERE captured_at < date_trunc('month', now())
            ORDER BY series_id, captured_at DESC
        ),
        first_in_month AS (
            SELECT DISTINCT ON (series_id) series_id, value
            FROM points
            WHERE captured_at >= date_trunc('month', now())
            ORDER BY series_id, captured_at ASC
        ),
        baseline AS (
            SELECT l.series_id, l.platform, COALESCE(b.value, f.value, 0) AS value
            FROM latest l
            LEFT JOIN before_month b ON b.series_id = l.series_id
            LEFT JOIN first_in_month f ON f.series_id = l.series_id
        )
"#;

/// How far back operator feedback is read.
///
/// The preference posterior decays on a 90-day half-life, so a decision from
/// a year ago carries about 6% of a fresh one's weight, and standing is a
/// statement about recent behaviour. Reading without a bound made the cost of
/// every autopilot cycle grow with the workspace's age, for signal that had
/// already decayed to nothing.
const OPERATOR_FEEDBACK_WINDOW_DAYS: u32 = 365;

/// Ceiling on operator-feedback rows per cycle. A workspace that somehow
/// produces more decisions than this in a year is one where the newest are
/// the ones worth reading, which is the order the query returns.
const OPERATOR_FEEDBACK_MAX_ROWS: i64 = 5_000;

/// The worker templates the brain may dispatch, in the order the evaluator
/// checks them.
///
/// Derived from [`WorkerTemplate::ALL`] rather than retyped. This was a
/// hand-written `&[&str]`, one of three such lists across three crates, and
/// `discord-poster` reached production present in this one and missing from
/// the other two.
fn worker_templates() -> Vec<&'static str> {
    WorkerTemplate::active()
        .into_iter()
        .map(|t| t.as_str())
        .collect()
}

/// How many insights ride along in a dispatch prompt.
///
/// Every insight now goes to every template rather than only to the one that
/// produced it, so this is a per-prompt budget and not a per-template one. It
/// is small on purpose: the insight block is context for the task, and a task
/// brief that opens with fifty prior findings is a worse brief than one that
/// opens with eight. The rest are not lost — they are the next eight once
/// these are consumed, because the query returns the newest unconsumed first
/// and consumption advances the window.
const INSIGHT_PROMPT_BUDGET: i64 = 8;

/// How far back `load_agent_execution_health` looks. Deliberately short
/// compared to the 60-day North Star window: a dead provider or exhausted
/// quota is an hours-scale fact, and averaging it over days would let a
/// six-hour outage hide inside a mostly-healthy week.
const AGENT_EXECUTION_HEALTH_WINDOW_HOURS: i64 = 6;

/// Assesses whether the worker layer is currently producing usable
/// outcomes, from the agent service's own task and outcome tables.
///
/// A task counts as bad when it failed outright, or when it completed but
/// every outcome it produced was rejected by the data-quality gate (most
/// commonly `NOT_GROUNDING_CHECKED` — the verifier never ran or never
/// passed). A task that completed and produced zero outcome rows at all
/// is not counted as bad: an empty/no-op run (a scan that found nothing)
/// is a valid observation, not a failure, and conflating the two is
/// exactly the mistake `agent_outcomes.rs`'s own data-quality guard exists
/// to avoid on the other side of this same boundary.
async fn load_agent_execution_health(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<crowdrelay_brain::AgentExecutionHealth, RepositoryError> {
    let since = now - time::Duration::hours(AGENT_EXECUTION_HEALTH_WINDOW_HOURS);
    let row: (i64, i64) = sqlx::query_as(
        r#"
        WITH task_window AS (
            SELECT id, status
            FROM agent_service_tasks
            WHERE workspace_id = $1
              AND created_at > $2
              AND status IN ('completed', 'failed')
        ),
        task_outcome_summary AS (
            SELECT task_id,
                   count(*) FILTER (WHERE status = 'processed') AS accepted,
                   count(*) FILTER (WHERE status = 'rejected') AS rejected
            FROM agent_outcomes
            WHERE workspace_id = $1
              AND task_id IN (SELECT id FROM task_window)
            GROUP BY task_id
        )
        SELECT
            count(*) AS attempted,
            count(*) FILTER (
                WHERE tw.status = 'failed'
                   OR (coalesce(tos.accepted, 0) = 0 AND coalesce(tos.rejected, 0) > 0)
            ) AS bad
        FROM task_window tw
        LEFT JOIN task_outcome_summary tos ON tos.task_id = tw.id
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(since)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)?;
    let (attempted, bad) = row;
    Ok(crowdrelay_brain::AgentExecutionHealth::assess(
        u32::try_from(attempted).unwrap_or(u32::MAX),
        u32::try_from(bad).unwrap_or(u32::MAX),
    ))
}

pub(in crate::autopilot) async fn load_growth_intelligence_snapshots(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Vec<GrowthIntelligenceSnapshot>, RepositoryError> {
    let pool = &repo.pool;

    // Load hours since last run per template from agent_service_tasks.
    // The agent_service_tasks table is owned by the TS agent service, but
    // the brain reads it to decide when to dispatch. This is a read-only
    // cross-service query — the brain never writes to agent_service_tasks
    // directly; the executor does that via the action dispatch.
    //
    // We distinguish two timestamps:
    // - `last_any_run`: the most recent task regardless of outcome. Used for
    //   the failed-run retry delay so the brain doesn't retry every cycle.
    // - `last_effective_run`: the most recent task whose outcome produced at
    //   least one item. The agents service writes one row per item with
    //   `payload.item` (singular); an empty run writes a single row with
    //   only `payload.rationale` and no `item` key. The cooldown is measured
    //   from the last effective run, so a failed/empty run does NOT reset
    //   the cooldown.
    //
    // `agent_service_tasks` belongs to the TypeScript agent service and no
    // migration here creates it, so on a fresh deployment it does not exist
    // yet. Querying it unguarded made the whole snapshot load fail, which meant
    // the brain could not form a world model at all until that service had
    // started — a startup order dependency nothing declared. "The table is not
    // there" and "the table is empty" mean the same thing to this query: no
    // template has run yet. Treat them the same.
    let tasks_table_exists =
        sqlx::query_scalar::<_, bool>("SELECT to_regclass('agent_service_tasks') IS NOT NULL")
            .fetch_one(pool)
            .await
            .map_err(map_sqlx)?;
    let last_runs: Vec<(String, Option<OffsetDateTime>, Option<OffsetDateTime>)> =
        if tasks_table_exists {
            sqlx::query_as(
                r#"
        SELECT ast.template_id,
               MAX(ast.created_at) AS last_any_run,
               MAX(CASE WHEN ao.payload ? 'item'
                        THEN ao.created_at END) AS last_effective_run
        FROM agent_service_tasks ast
        LEFT JOIN agent_outcomes ao ON ao.task_id = ast.id
        WHERE ast.workspace_id = $1
        GROUP BY ast.template_id
        "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_all(pool)
            .await
            .map_err(map_sqlx)?
        } else {
            tracing::info!("agent_service_tasks is absent; treating every template as never run");
            Vec::new()
        };

    // Load workspace situation: upcoming events, fan growth, unengaged targets.
    // Only `published` events are publicly announced and promotable; the
    // `events` table has no `scheduled` status (valid: draft/published/
    // cancelled/completed), so filtering by `published` + `starts_at > now()`
    // gives us the next real upcoming show.
    let upcoming_event: Option<(Option<OffsetDateTime>,)> = sqlx::query_as(
        r#"
        SELECT MIN(starts_at) FROM events
        WHERE workspace_id = $1 AND starts_at > now() AND status = 'published'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx)?;

    // Load the communities the growth loop may engage, with the audience
    // graph's measurements and each community's own promotion rules. See
    // `community_targets` for why this is not three columns off
    // `agent_outreach_targets` any more. The count of promoted targets is
    // derived from this query's row count, so we don't need a separate
    // count query.
    let unengaged_targets = community_targets::load_community_targets(pool, workspace_id).await?;

    let next_event_time = upcoming_event.and_then(|(t,)| t);
    let has_upcoming_event = next_event_time
        .map(|t| (t - now).whole_days())
        .is_some_and(|d| (0..=30).contains(&d));
    let days_to_next_event = next_event_time
        .map(|t| (t - now).whole_days())
        .filter(|d| *d >= 0)
        .map(|d| d as u32);

    // Load unconsumed insights from agent_outcomes. The brain feeds these
    // into the next worker dispatch prompt ("here's what we already know")
    // and marks them consumed after planning. This closes the feedback loop.
    //
    // Routed by kind, not by the template that produced them. All three kinds
    // read here are statements about the workspace — what a campaign did, what
    // a release needs, what somebody noticed — and none of them is a fact only
    // one worker can use. A note about which subreddits answer press angles is
    // worth as much to the press pitcher as to the scanner that found it.
    //
    // Routing by producer instead had two failure modes, and the second was
    // the expensive one. A snapshot exists only for an *active* template, so
    // an insight from a disabled template reached no snapshot and no prompt;
    // and because only insights that reach a snapshot are ever marked
    // consumed, and retention deletes consumed rows only, such a row stayed
    // forever while still occupying a slot in this window. Past the limit the
    // window held nothing deliverable at all: the block the worker received
    // went empty while every log line still said insights were loaded. Routing
    // by kind removes the class — every row loaded here is deliverable to
    // whatever dispatches next, so nothing can accumulate undeliverable.
    //
    // `template_id` is still selected, as provenance shown to the worker
    // ("this came from the Reddit scanner"), and the join stays LEFT so an
    // outcome whose task row is gone keeps its insight instead of losing it.
    const INSIGHTS_WITH_TEMPLATE: &str = r#"
            SELECT ao.id,
                   COALESCE(ast.template_id, 'unknown') AS template_id,
                   ao.kind,
                   COALESCE(ao.payload->'item'->>'headline', ao.payload->'item'->>'subject', '(no headline)') AS headline,
                   COALESCE(ao.payload->'item'->>'detail', ao.payload->'item'->>'body', '') AS detail,
                   ao.payload->'item'->>'recommended_action' AS recommended_action
            FROM agent_outcomes ao
            LEFT JOIN agent_service_tasks ast ON ast.id = ao.task_id
            WHERE ao.workspace_id = $1
              AND ao.status = 'processed'
              AND ao.consumed_at IS NULL
              AND ao.kind IN ('campaign_insight', 'generic_insight', 'release_plan_note')
            ORDER BY ao.created_at DESC
            LIMIT $2
            "#;
    // Without the agent service's task table there is no provenance to show,
    // but the insights themselves are still worth delivering, so the same read
    // runs without the join.
    const INSIGHTS_WITHOUT_TEMPLATE: &str = r#"
            SELECT ao.id,
                   'unknown'::text AS template_id,
                   ao.kind,
                   COALESCE(ao.payload->'item'->>'headline', ao.payload->'item'->>'subject', '(no headline)') AS headline,
                   COALESCE(ao.payload->'item'->>'detail', ao.payload->'item'->>'body', '') AS detail,
                   ao.payload->'item'->>'recommended_action' AS recommended_action
            FROM agent_outcomes ao
            WHERE ao.workspace_id = $1
              AND ao.status = 'processed'
              AND ao.consumed_at IS NULL
              AND ao.kind IN ('campaign_insight', 'generic_insight', 'release_plan_note')
            ORDER BY ao.created_at DESC
            LIMIT $2
            "#;
    let insights: Vec<(uuid::Uuid, String, String, String, String, Option<String>)> =
        if tasks_table_exists {
            sqlx::query_as(INSIGHTS_WITH_TEMPLATE)
                .bind(workspace_id.into_uuid())
                .bind(INSIGHT_PROMPT_BUDGET)
                .fetch_all(pool)
                .await
                .map_err(map_sqlx)?
        } else {
            sqlx::query_as(INSIGHTS_WITHOUT_TEMPLATE)
                .bind(workspace_id.into_uuid())
                .bind(INSIGHT_PROMPT_BUDGET)
                .fetch_all(pool)
                .await
                .map_err(map_sqlx)?
        };

    let recent_insights: Vec<RecentInsight> = insights
        .into_iter()
        .map(
            |(outcome_id, template_id, kind, headline, detail, recommended_action)| RecentInsight {
                outcome_id,
                template_id,
                kind,
                headline,
                detail,
                recommended_action,
            },
        )
        .collect();

    // Load community engagement history: aggregated post performance per
    // subreddit from `community_post_metrics`. Only the latest metrics row
    // per post is used, averaged across all posts to each subreddit in the
    // last 30 days. This gives the brain a signal: "r/abc gets 45 upvotes
    // on average, r/xyz gets 0 — don't waste LLM budget there."
    let engagement_rows: Vec<(String, i64, f64, f64, f64, Option<f64>)> = sqlx::query_as(
        r#"
        WITH latest_per_post AS (
            SELECT DISTINCT ON (cpm.community_post_id)
                cpm.community_post_id,
                cpm.score,
                cpm.upvotes,
                cpm.num_comments,
                cpm.upvote_ratio,
                cp.subreddit
            FROM community_post_metrics cpm
            JOIN community_posts cp ON cp.id = cpm.community_post_id
            WHERE cp.workspace_id = $1
              AND cp.posted_at > now() - interval '30 days'
            ORDER BY cpm.community_post_id, cpm.measured_at DESC
        )
        SELECT subreddit,
               COUNT(*)::bigint AS post_count,
               AVG(score)::double precision AS avg_score,
               AVG(upvotes)::double precision AS avg_upvotes,
               AVG(num_comments)::double precision AS avg_comments,
               AVG(upvote_ratio)::double precision AS avg_upvote_ratio
        FROM latest_per_post
        GROUP BY subreddit
        ORDER BY avg_score DESC
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    let engagement_history: Vec<CommunityEngagementSummary> = engagement_rows
        .into_iter()
        .map(
            |(subreddit, post_count, avg_score, avg_upvotes, avg_comments, avg_upvote_ratio)| {
                CommunityEngagementSummary {
                    subreddit,
                    post_count: u32::try_from(post_count.max(0)).unwrap_or(0),
                    avg_score,
                    avg_upvotes,
                    avg_comments,
                    avg_upvote_ratio,
                }
            },
        )
        .collect();

    // Standings and tenant preference, both derived from the same bounded
    // window of operator feedback and measurement outcomes.
    let worker_signals::WorkerSignals {
        standings,
        tenant_preference,
    } = worker_signals::load_worker_signals(pool, workspace_id).await?;

    // ── World Model data ──
    // The brain's belief about the world: fan counts, signal installs,
    // community reach, outreach pipeline, and growth target progress.
    // Loaded once and shared across all template snapshots. The recent_fans
    // count (last 14 days) is merged into this query to save a round-trip.
    //
    // The five count queries are independent — parallelize them to reduce
    // cycle latency. Each hits a different table, so they contend on no
    // shared lock and the pool serves them concurrently.
    let (fan_counts, signal_counts, community_counts, outreach_counts, north_star_str) =
        tokio::try_join!(
            sqlx::query_as::<_, (i64, i64, i64, i64)>(
                r#"
                SELECT
                    COUNT(*)::bigint AS total_fans,
                    COUNT(*) FILTER (WHERE created_at > date_trunc('month', now()))::bigint AS fans_this_month,
                    COUNT(*) FILTER (WHERE created_at > now() - interval '30 days'
                                     AND created_at <= now() - interval '14 days')::bigint AS fans_prev_window,
                    COUNT(*) FILTER (WHERE created_at > now() - interval '14 days')::bigint AS recent_fans
                FROM fans
                WHERE workspace_id = $1 AND status != 'suppressed'
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_one(pool),
            sqlx::query_as::<_, (i64, i64)>(
                r#"
                -- People, not devices.
                --
                -- This counted endpoint rows, and one fan with a phone and two
                -- browsers is three rows. The north star read that count while
                -- its target came from the fan count -- `(total_fans/10).max(5)`
                -- -- so the measure and the goal were in different units, and
                -- `signal_conversion_rate_bps` divided devices by fans and
                -- capped the result at 100% to hide the overflow. That cap is
                -- the fingerprint of the bug: a genuine fraction of fans cannot
                -- exceed one.
                --
                -- Production carries 13 endpoint rows from 2 distinct fans, so
                -- the brain was optimizing a number inflated several-fold over
                -- the thing it means to grow.
                SELECT
                    COUNT(DISTINCT fan_id)::bigint AS total_installs,
                    COUNT(DISTINCT fan_id) FILTER (WHERE created_at > date_trunc('month', now()))::bigint AS installs_this_month
                FROM fan_push_endpoints
                WHERE workspace_id = $1 AND active = true AND invalidated_at IS NULL
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_one(pool),
            sqlx::query_as::<_, (i64, i64)>(
                r#"
                SELECT
                    COUNT(DISTINCT dp.id)::bigint AS discovered,
                    COUNT(DISTINCT cp.subreddit)::bigint AS active
                FROM discovery_places dp
                LEFT JOIN community_posts cp ON cp.subreddit = dp.name
                    AND cp.workspace_id = dp.workspace_id
                    AND cp.posted_at > now() - interval '30 days'
                WHERE dp.workspace_id = $1 AND dp.status = 'active'
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_one(pool),
            sqlx::query_as::<_, (i64, i64, i64)>(
                r#"
                SELECT
                    COUNT(DISTINCT ot.id) FILTER (WHERE ot.status = 'proposed')::bigint AS pending,
                    COUNT(DISTINCT ot.id) FILTER (WHERE ot.status = 'promoted')::bigint AS promoted,
                    COUNT(DISTINCT cp.target_id)::bigint AS engaged
                FROM agent_outreach_targets ot
                LEFT JOIN community_posts cp ON cp.target_id = ot.id
                    AND cp.workspace_id = ot.workspace_id
                    AND cp.status = 'posted'
                WHERE ot.workspace_id = $1
                "#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_one(pool),
            // North star metric from tenant_settings — independent of all
            // count queries, so it runs in parallel with them.
            sqlx::query_as::<_, (String,)>(
                r#"SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = 'north_star_metric'"#,
            )
            .bind(workspace_id.into_uuid())
            .fetch_optional(pool),
        )
        .map_err(map_sqlx)?;

    let total_fans = u32::try_from(fan_counts.0.max(0)).unwrap_or(0);
    let fans_this_month = u32::try_from(fan_counts.1.max(0)).unwrap_or(0);
    let fans_prev_window = u32::try_from(fan_counts.2.max(0)).unwrap_or(0);
    let fan_growth_stagnant = fan_counts.3 == 0;

    let total_signal_installs = u32::try_from(signal_counts.0.max(0)).unwrap_or(0);
    let signal_installs_this_month = u32::try_from(signal_counts.1.max(0)).unwrap_or(0);

    let discovered_communities = u32::try_from(community_counts.0.max(0)).unwrap_or(0);
    let active_communities = u32::try_from(community_counts.1.max(0)).unwrap_or(0);

    let pending_outreach_targets = u32::try_from(outreach_counts.0.max(0)).unwrap_or(0);
    let promoted_outreach_targets = u32::try_from(outreach_counts.1.max(0)).unwrap_or(0);
    let engaged_outreach_targets = u32::try_from(outreach_counts.2.max(0)).unwrap_or(0);

    // Compute growth trend from fan counts.
    let fan_growth_trend = if fan_growth_stagnant {
        GrowthTrend::Stagnant
    } else if fans_prev_window == 0 {
        GrowthTrend::Accelerating
    } else if fans_this_month as u64 * 2 > fans_prev_window as u64 * 3 {
        // This month's pace > 1.5x previous window → accelerating
        GrowthTrend::Accelerating
    } else if fans_this_month as u64 * 3 < fans_prev_window as u64 * 2 {
        // This month's pace < 0.67x previous window → decelerating
        GrowthTrend::Decelerating
    } else {
        GrowthTrend::Steady
    };

    // Compute fan growth rate (basis points, monthly).
    let fan_growth_rate_bps = if total_fans == 0 {
        0
    } else {
        u16::try_from((u64::from(fans_this_month) * 10_000 / u64::from(total_fans)).min(10_000))
            .unwrap_or(10_000)
    };

    // Signal conversion rate: fraction of fans with Signal installed.
    let signal_conversion_rate_bps = if total_fans == 0 {
        0
    } else {
        u16::try_from(
            (u64::from(total_signal_installs) * 10_000 / u64::from(total_fans)).min(10_000),
        )
        .unwrap_or(10_000)
    };

    // Average community engagement (upvote ratio in basis points).
    let avg_community_engagement_bps = if engagement_history.is_empty() {
        0
    } else {
        let avg_ratio: f64 = engagement_history
            .iter()
            .filter_map(|e| e.avg_upvote_ratio)
            .map(|r| r.clamp(0.0, 1.0))
            .sum::<f64>()
            / engagement_history
                .iter()
                .filter(|e| e.avg_upvote_ratio.is_some())
                .count()
                .max(1) as f64;
        u16::try_from((avg_ratio * 10_000.0) as u64).unwrap_or(0)
    };

    // engagement_history is ordered by avg_score DESC (from the SQL query),
    // so first = best, last = worst.
    let best_performing_community = engagement_history.first().map(|e| e.subreddit.clone());
    let worst_performing_community = engagement_history.last().map(|e| e.subreddit.clone());

    // North star metric — loaded in parallel with the count queries above.
    let north_star = north_star_str
        .and_then(|(s,)| NorthStarMetric::parse(&s))
        .unwrap_or_default();

    // Load the current value for the north star metric from the growth metric
    // series. For SignalInstalls we use the signal install counts (already
    // loaded). For the others we read the platform's own series.
    //
    // Points store absolute levels, never deltas (see 0073), so "this month"
    // is a subtraction: level now minus level at the start of the month. A
    // workspace may run several accounts on one platform (two YouTube
    // channels), so levels are summed across that platform's series.
    let (north_star_current, north_star_this_month) =
        match (north_star.platform(), north_star.metric_key()) {
            (Some(platform), Some(metric_key)) => {
                let metric_counts: (i64, i64) = sqlx::query_as(
                    r#"
                WITH target_series AS (
                    SELECT id FROM viryaos_growth_metric_series
                    WHERE workspace_id = $1
                      AND platform = $2
                      AND metric_key = $3
                      AND active
                ),
                points AS (
                    SELECT p.series_id, p.captured_at, p.value
                    FROM viryaos_growth_metric_points p
                    JOIN target_series s ON s.id = p.series_id
                    WHERE p.workspace_id = $1
                ),
                latest AS (
                    SELECT DISTINCT ON (series_id) series_id, value
                    FROM points
                    ORDER BY series_id, captured_at DESC
                ),
                before_month AS (
                    SELECT DISTINCT ON (series_id) series_id, value
                    FROM points
                    WHERE captured_at < date_trunc('month', now())
                    ORDER BY series_id, captured_at DESC
                ),
                first_in_month AS (
                    SELECT DISTINCT ON (series_id) series_id, value
                    FROM points
                    WHERE captured_at >= date_trunc('month', now())
                    ORDER BY series_id, captured_at ASC
                ),
                baseline AS (
                    -- Level this series stood at when the month opened. A
                    -- series first observed this month falls back to its own
                    -- first reading, so connecting an account mid-month does
                    -- not report its whole existing audience as won this month.
                    SELECT l.series_id, COALESCE(b.value, f.value, 0) AS value
                    FROM latest l
                    LEFT JOIN before_month b ON b.series_id = l.series_id
                    LEFT JOIN first_in_month f ON f.series_id = l.series_id
                )
                SELECT
                    COALESCE((SELECT SUM(value) FROM latest), 0)::bigint,
                    GREATEST(
                        COALESCE((SELECT SUM(value) FROM latest), 0)
                            - COALESCE((SELECT SUM(value) FROM baseline), 0),
                        0
                    )::bigint
                "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(platform.as_str())
                .bind(metric_key)
                .fetch_optional(pool)
                .await
                .map_err(map_sqlx)?
                .unwrap_or((0, 0));
                (
                    u32::try_from(metric_counts.0.max(0)).unwrap_or(u32::MAX),
                    u32::try_from(metric_counts.1.max(0)).unwrap_or(u32::MAX),
                )
            }
            // SignalInstalls, TotalAudience, and any future north star with no
            // single platform series. TotalAudience is resolved below, once the
            // aggregate it names has actually been summed.
            _ => (total_signal_installs, signal_installs_this_month),
        };

    // Off-platform audience across every connected feed.
    //
    // The north star above reads one platform. This reads all of them, using
    // each platform's audience-size key so plays are never added to people.
    // Same baseline rule as the north star: points hold absolute levels, so
    // "this month" is latest minus the level at the month boundary, and a
    // series first seen this month falls back to its own first reading.
    let audience_keys: Vec<(&str, &str)> = MetricPlatform::ALL
        .into_iter()
        .filter(|platform| platform.is_off_platform_feed())
        .filter_map(|platform| {
            platform
                .audience_metric_key()
                .map(|key| (platform.as_str(), key))
        })
        .collect();
    let audience_platforms: Vec<&str> = audience_keys.iter().map(|(p, _)| *p).collect();
    let audience_metric_keys: Vec<&str> = audience_keys.iter().map(|(_, k)| *k).collect();
    // Per-platform ranking wants Signal alongside the off-platform feeds; the
    // aggregate above deliberately excludes it, because `off_platform_audience`
    // means audience that is not already ours.
    let mut growth_platforms = audience_platforms.clone();
    let mut growth_metric_keys = audience_metric_keys.clone();
    growth_platforms.push("signal");
    growth_metric_keys.push("active_fans");

    let audience_row: (i64, i64, i64, i64);
    let platform_growth_rows: Vec<(String, i64, i64)>;
    let hypothesis_states: std::collections::HashMap<
        String,
        crowdrelay_brain::hypothesis::HypothesisState,
    >;
    // The aggregate and per-platform audience queries use the same CTE
    // shape but different platform lists and SELECT clauses. They are
    // independent — parallelize them with the hypothesis state load to
    // halve the audience-query latency.
    {
        let audience_sql = format!(
            "{AUDIENCE_WINDOW_CTE}
            SELECT
                COALESCE((SELECT SUM(value) FROM latest), 0)::bigint,
                GREATEST(
                    COALESCE((SELECT SUM(value) FROM latest), 0)
                        - COALESCE((SELECT SUM(value) FROM baseline), 0),
                    0
                )::bigint,
                (SELECT COUNT(DISTINCT platform) FROM latest)::bigint,
                (SELECT COUNT(DISTINCT platform) FROM latest
                 WHERE captured_at > now() - interval '7 days')::bigint
            "
        );
        let platform_sql = format!(
            "{AUDIENCE_WINDOW_CTE}
            SELECT
                latest.platform,
                SUM(latest.value)::bigint AS audience,
                GREATEST(SUM(latest.value) - COALESCE(SUM(baseline.value), 0), 0)::bigint AS gained
            FROM latest
            LEFT JOIN baseline ON baseline.series_id = latest.series_id
            GROUP BY latest.platform
            "
        );
        let audience_fut = async {
            sqlx::query_as::<_, (i64, i64, i64, i64)>(&audience_sql)
                .bind(workspace_id.into_uuid())
                .bind(&audience_platforms)
                .bind(&audience_metric_keys)
                .fetch_optional(pool)
                .await
                .map_err(map_sqlx)
        };
        // The same audience arithmetic, split by platform instead of summed.
        //
        // The aggregate above says whether the audience is growing; this says
        // where. Nothing read that before, so the strategy's template order stayed
        // the fixed guess it was written as, however the numbers moved.
        //
        // Signal is deliberately included even though it is not an off-platform
        // feed: an install is the most valuable unit the North Star has, and
        // leaving it out would rank every platform except the one that matters
        // most.
        let platform_fut = async {
            sqlx::query_as::<_, (String, i64, i64)>(&platform_sql)
                .bind(workspace_id.into_uuid())
                .bind(&growth_platforms)
                .bind(&growth_metric_keys)
                .fetch_all(pool)
                .await
                .map_err(map_sqlx)
        };
        let (audience_opt, platform_rows, hyp_states) = tokio::try_join!(
            audience_fut,
            platform_fut,
            load_hypothesis_states(repo, workspace_id)
        )?;
        audience_row = audience_opt.unwrap_or((0, 0, 0, 0));
        platform_growth_rows = platform_rows;
        hypothesis_states = hyp_states;
    }
    let platform_growth: Vec<PlatformGrowth> = platform_growth_rows
        .into_iter()
        .map(|(platform, audience, gained)| PlatformGrowth {
            platform,
            audience: u32::try_from(audience.max(0)).unwrap_or(u32::MAX),
            gained_this_month: u32::try_from(gained.max(0)).unwrap_or(u32::MAX),
        })
        .collect();

    let off_platform_audience = u32::try_from(audience_row.0.max(0)).unwrap_or(u32::MAX);
    let off_platform_audience_this_month = u32::try_from(audience_row.1.max(0)).unwrap_or(u32::MAX);
    let connected_platforms = u32::try_from(audience_row.2.max(0)).unwrap_or(u32::MAX);
    let fresh_platforms = u32::try_from(audience_row.3.max(0)).unwrap_or(u32::MAX);

    // Growth target progress.
    // A tenant whose north star is the whole portfolio reads the aggregate just
    // computed rather than any one platform. Resolved here because the sum does
    // not exist until the query above has run.
    let (north_star_current, north_star_this_month) = if north_star.is_total_audience() {
        (off_platform_audience, off_platform_audience_this_month)
    } else {
        (north_star_current, north_star_this_month)
    };

    let growth_target = GrowthTarget::from_fan_count(total_fans, north_star, north_star_current);
    let growth_target_progress = GrowthTargetProgress::from_counts(
        growth_target,
        fans_this_month,
        signal_installs_this_month,
        north_star,
        north_star_this_month,
    );

    let world_model = WorldModel {
        total_fans,
        fans_this_month,
        fan_growth_rate_bps,
        fan_growth_trend,
        total_signal_installs,
        signal_installs_this_month,
        signal_conversion_rate_bps,
        north_star,
        north_star_current,
        north_star_this_month,
        off_platform_audience,
        off_platform_audience_this_month,
        connected_platforms,
        fresh_platforms,
        platform_growth,
        discovered_communities,
        active_communities,
        avg_community_engagement_bps,
        best_performing_community,
        worst_performing_community,
        pending_outreach_targets,
        promoted_outreach_targets,
        engaged_outreach_targets,
        days_to_next_event,
        has_upcoming_event,
        growth_target_progress,
    };

    // Agent execution health: is the worker layer currently producing
    // usable outcomes? Computed once, workspace-wide — same value on every
    // snapshot, mirroring how metacognition is one state per tenant, not
    // per template. Reuses the same existence guard as the other
    // agent_service_tasks reads above: the table belongs to the TypeScript
    // agent service and is absent on a fresh deployment.
    let agent_execution_health = if tasks_table_exists {
        load_agent_execution_health(pool, workspace_id, now).await?
    } else {
        crowdrelay_brain::AgentExecutionHealth::Unknown
    };
    if agent_execution_health.needs_attention() {
        tracing::warn!(
            health = agent_execution_health.as_str(),
            "agent execution health degraded — dispatch budget for agent-run templates reduced"
        );
    }

    // Build one snapshot per worker template.
    let templates = worker_templates();
    // hypothesis_states loaded in parallel with the audience queries above.
    let mut snapshots = Vec::with_capacity(templates.len());
    for template_id in &templates {
        let (hours_since_last_run, hours_since_last_effective_run) = last_runs
            .iter()
            .find(|(tid, _, _)| tid == template_id)
            .map(|(_, last_any, last_effective)| {
                let any = last_any.map(|t| {
                    let delta = now - t;
                    u32::try_from(delta.whole_hours().max(0)).unwrap_or(u32::MAX)
                });
                let effective = last_effective.map(|t| {
                    let delta = now - t;
                    u32::try_from(delta.whole_hours().max(0)).unwrap_or(u32::MAX)
                });
                (any, effective)
            })
            .unwrap_or((None, None));

        // Every template gets every insight. The three kinds loaded above are
        // statements about the workspace rather than about the worker that
        // happened to notice them, so restricting one to its producer withheld
        // it from every other worker that could have used it. Already capped
        // to `INSIGHT_PROMPT_BUDGET` at load, so this clone is bounded.
        let template_insights = recent_insights.clone();

        // Attach engagement history and unengaged targets only to the
        // community-engager snapshot. Other templates don't use them, so
        // we avoid cloning the Vecs.
        let history = if *template_id == "community-engager" {
            engagement_history.clone()
        } else {
            Vec::new()
        };
        let targets = if *template_id == "community-engager" {
            unengaged_targets.clone()
        } else {
            Vec::new()
        };

        snapshots.push(GrowthIntelligenceSnapshot {
            template_id: (*template_id).to_owned(),
            hours_since_last_run,
            hours_since_last_effective_run,
            has_upcoming_event,
            days_to_next_event,
            fan_growth_stagnant,
            unengaged_outreach_targets: promoted_outreach_targets,
            unengaged_targets: targets,
            recent_insights: template_insights,
            community_engagement_history: history,
            // The measured standing from past dispatch outcomes. Workers
            // with no measured outcomes are untested (run at base cadence).
            standing: standings
                .get(*template_id)
                .copied()
                .unwrap_or(Standing::Untested { measured: 0 }),
            world_model: world_model.clone(),
            tenant_preference: tenant_preference.clone(),
            // Hypothesis lifecycle: loaded from viryaos_growth_hypotheses.
            // Templates without a persisted state default to Active,
            // preserving current behavior. The evaluator uses may_act()
            // to gate dispatch and sizing_multiplier() to scale budget.
            hypothesis_state: hypothesis_states
                .get(*template_id)
                .copied()
                .unwrap_or(crowdrelay_brain::hypothesis::HypothesisState::Active),
            // Metacognition: assess the brain's own performance from
            // the daily North Star series. The assessment feeds
            // exploration_boost into EFE weights and sizing_multiplier
            // into dispatch budget — the brain explores harder when
            // stagnant and sizes down when regressing, mirroring Kern's
            // metacognition feedback loop.
            //
            // Falls back to Improving (neutral) when there is not yet
            // enough history to assess — the honest answer for a young
            // system rather than claiming stagnation.
            //
            // Change-point detection: CUSUM runs on the same daily
            // North Star series to detect sudden regime shifts (viral
            // moments, algorithm changes, audience fatigue). The
            // detected change points are logged so the operator can
            // see when the brain detected a shift, and the last
            // change point's direction feeds into the self-assessment:
            // an upward shift boosts toward Improving, a downward
            // shift triggers Regressing earlier than the proportional
            // threshold would.
            agent_execution_health,
            metacognition: {
                let samples = super::super::daily_north_star(
                    repo.pool(),
                    workspace_id,
                    super::super::NORTH_STAR_WINDOW_DAYS,
                )
                .await
                .unwrap_or_default();
                let state = crowdrelay_brain::self_assessment::assess(samples.clone());
                // Run CUSUM change-point detection on the North Star
                // series. The detector is created fresh each cycle —
                // it is stateless across cycles, so this is a batch
                // detection over the full window. The threshold and
                // drift are tuned for daily fan counts.
                let series: Vec<f64> = samples.iter().map(|s| s.value).collect();
                let shifts = crowdrelay_brain::change_point::detect_fan_growth_shifts(
                    &series, 10.0, // threshold: 10 fans cumulative deviation
                    2.0,  // drift: 2 fans of noise tolerated
                );
                if let Some(last) = shifts.last() {
                    tracing::info!(
                        workspace_id = %workspace_id.into_uuid(),
                        direction = last.direction.as_str(),
                        shift_size = last.shift_size(),
                        pre_mean = last.pre_mean,
                        post_mean = last.post_mean,
                        total_shifts = shifts.len(),
                        "change-point detection: regime shift detected in North Star series"
                    );
                }
                let mut m = crowdrelay_brain::self_assessment::MetacognitionMonitor::new();
                m.observe(state);
                m
            },
        });
    }

    tracing::info!(
        snapshot_count = snapshots.len(),
        templates = ?snapshots.iter().map(|s| (s.template_id.clone(), s.hours_since_last_effective_run, s.hours_since_last_run, s.standing.is_retired())).collect::<Vec<_>>(),
        "GI snapshots loaded"
    );

    Ok(snapshots)
}

/// Marks agent outcomes as consumed by the brain. Called after the evaluator
/// has factored the insights into its dispatch decisions. Consumed rows are
/// deleted by the retention worker after 7 days.
pub(in crate::autopilot) async fn mark_insights_consumed(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    outcome_ids: &[uuid::Uuid],
) -> Result<u64, RepositoryError> {
    if outcome_ids.is_empty() {
        return Ok(0);
    }
    let pool = &repo.pool;
    let result = sqlx::query(
        r#"
        UPDATE agent_outcomes
        SET consumed_at = now()
        WHERE workspace_id = $1
          AND id = ANY($2)
          AND consumed_at IS NULL
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(outcome_ids)
    .execute(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(result.rows_affected())
}

/// Loads the causal model from past dispatch predictions and their measured
/// outcomes. The brain uses this to predict how many fans each worker
/// dispatch will produce.
///
/// # Checkpoint + Delta Replay
///
/// The brain loads a serialized checkpoint from `viryaos_brain_state` on
/// startup, then applies only delta evidence (evidence with timestamp >
/// checkpoint timestamp) from `viryaos_growth_evidence`. This is O(delta)
/// instead of O(full history) every cycle.
///
/// If no checkpoint exists, the brain falls back to full replay from the
/// evidence table (or the legacy `viryaos_brain_evidence` view for
/// backward compatibility).
///
/// In addition to the outcome model P(Y|action,context), this function also
/// loads treatment-effect observations and updates the treatment-effect
/// posterior P(τ|context). When enough paired experiment data has
/// accumulated, the brain uses τ as the primary ranking signal.
pub(in crate::autopilot) async fn load_causal_model(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<LoadedCausalModel, RepositoryError> {
    use crowdrelay_brain::CausalModel;

    // Try to load a brain state checkpoint for fast startup.
    let checkpoint = super::evidence::load_brain_state(repo, workspace_id, "causal_model").await?;
    let (model, belief) = if let Some((state_json, checkpoint_time)) = checkpoint {
        // Hashed before deserialization, from the bytes the row actually held.
        // `viryaos_brain_state` keeps one row per module and updates it in
        // place, so `checkpoint_time` says when and cannot say which — the
        // state it labelled is gone by the next cycle. The content hash stays
        // true after the row moves on.
        let checkpoint_content_hash = checkpoint_content_hash(&state_json);
        match serde_json::from_value::<CausalModel>(state_json) {
            Ok(mut model) => {
                // Load only delta evidence since the checkpoint.
                let delta = super::evidence::load_growth_evidence(
                    repo,
                    workspace_id,
                    Some(checkpoint_time),
                )
                .await?;
                // The control arm of every experiment this batch touches,
                // fetched by experiment rather than by cursor. A treated row
                // whose control resolved in an earlier batch still has a
                // counterfactual; without this it arrived alone, was capped at
                // quasi-experimental, and contributed a raw pre/post
                // difference. These rows are a contrast only — they were
                // learned from when they first resolved.
                let experiments: Vec<uuid::Uuid> = delta
                    .iter()
                    .filter(|ev| ev.treatment.is_treatment())
                    .filter_map(|ev| ev.experiment_uuid)
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                let contrast =
                    super::evidence::load_control_arm_evidence(repo, workspace_id, &experiments)
                        .await?;
                apply_evidence_to_model_with_contrast(
                    &mut model,
                    &delta,
                    &contrast,
                    Some(checkpoint_time),
                );
                // Also apply delta evidence to the strategy posterior so it
                // stays in sync with the causal model's evidence replay —
                // including the per-horizon gating, which is why the
                // checkpoint goes across too.
                apply_evidence_to_stored_strategy_posterior(
                    repo,
                    workspace_id,
                    &delta,
                    PosteriorReplay::Delta,
                    Some(checkpoint_time),
                )
                .await;
                // Attribution: summarize the delta evidence for operator
                // observability. Read-only — does not mutate any posterior.
                let attr = crowdrelay_brain::attribution::attribute_fan_growth(&delta);
                tracing::info!(
                    delta_evidence = delta.len(),
                    contrast_evidence = contrast.len(),
                    experiments = experiments.len(),
                    checkpoint_time = %checkpoint_time,
                    total_observed_fans = attr.total_observed_fans,
                    total_incremental_fans = attr.total_incremental_fans,
                    total_durable_fans = attr.total_durable_fans,
                    resolved_observations = attr.resolved_observations,
                    partial_observations = attr.partial_observations,
                    "loaded causal model from checkpoint + delta"
                );
                // Distinct from the load-summary line above: that one fires
                // every cycle, empty or not, and reading it for "did the
                // brain learn anything" means diffing evidence counts across
                // log lines by hand. This one fires only when the model's
                // posteriors actually moved, so grepping for it answers the
                // question directly — the proof-of-learning line the North
                // Star audit needed and the load summary alone could not
                // give.
                if !delta.is_empty() {
                    tracing::info!(
                        prior_checkpoint_hash = %checkpoint_content_hash,
                        evidence_applied = delta.len(),
                        contrast_evidence = contrast.len(),
                        total_incremental_fans = attr.total_incremental_fans,
                        total_durable_fans = attr.total_durable_fans,
                        "brain learned: posterior updated"
                    );
                }
                let belief = BeliefStateOrigin::Checkpoint {
                    checkpoint_content_hash,
                    checkpoint_updated_at: checkpoint_time,
                    // The estimate came from the checkpoint plus these, not
                    // from the checkpoint alone.
                    delta_evidence: u32::try_from(delta.len()).unwrap_or(u32::MAX),
                };
                (model, belief)
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to deserialize brain checkpoint, falling back to full replay");
                full_replay_with_origin(repo, workspace_id).await?
            }
        }
    } else {
        // No checkpoint — full replay from evidence table or legacy view.
        full_replay_with_origin(repo, workspace_id).await?
    };

    Ok(LoadedCausalModel { model, belief })
}

/// A full replay and the honest statement that no checkpoint produced it.
///
/// Reporting a checkpoint identity here would be a fabricated one: there was
/// no checkpoint, and the model is whatever the evidence table rebuilt.
async fn full_replay_with_origin(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<(crowdrelay_brain::CausalModel, BeliefStateOrigin), RepositoryError> {
    let (model, evidence_replayed) = full_replay(repo, workspace_id).await?;
    Ok((model, BeliefStateOrigin::FullReplay { evidence_replayed }))
}

/// The identity of a stored checkpoint: a hash of the state the row held.
///
/// Not the row's `updated_at`, which the next cycle overwrites, and not a new
/// column. Identical belief state hashes identically; a different one does
/// not; and neither answer changes when the row is later replaced.
fn checkpoint_content_hash(state: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};

    // `serde_json` orders object keys, so serializing is already canonical for
    // a given value — the same state always produces the same bytes.
    let canonical = serde_json::to_string(state).unwrap_or_default();
    let digest = Sha256::digest(canonical.as_bytes());
    let mut identity = String::from("sha256:");
    for byte in digest.iter().take(16) {
        identity.push_str(&format!("{byte:02x}"));
    }
    identity
}

/// Saves a causal model checkpoint to the brain state table for fast
/// startup with delta replay. Called after each autopilot cycle.
pub(in crate::autopilot) async fn save_causal_model_checkpoint(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    model: &crowdrelay_brain::CausalModel,
) -> Result<(), RepositoryError> {
    match serde_json::to_value(model) {
        Ok(state) => {
            super::evidence::save_brain_state(repo, workspace_id, "causal_model", &state).await
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize causal model checkpoint");
            Err(RepositoryError::Unexpected)
        }
    }
}

/// Full replay from the growth evidence table, falling back to the legacy
/// brain_evidence view when the table has no data.
///
/// Returns the model and the number of evidence rows replayed, so the caller
/// can report the count without loading the evidence table a second time.
async fn full_replay(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<(crowdrelay_brain::CausalModel, u32), RepositoryError> {
    use crowdrelay_brain::{CausalModel, DispatchPrediction, PredictionOutcome};

    // Try the new growth evidence table first.
    let evidence = super::evidence::load_growth_evidence(repo, workspace_id, None).await?;
    let evidence_replayed = u32::try_from(evidence.len()).unwrap_or(u32::MAX);
    if !evidence.is_empty() {
        let mut model = CausalModel::default();
        apply_evidence_to_model(&mut model, &evidence);
        // Also replay evidence into the strategy posterior from scratch — this
        // is every row, so it rebuilds rather than accumulates.
        apply_evidence_to_stored_strategy_posterior(
            repo,
            workspace_id,
            &evidence,
            PosteriorReplay::FromScratch,
            None,
        )
        .await;
        // Attribution: summarize where fan growth came from, for operator
        // observability. Read-only — does not mutate any posterior.
        let attr = crowdrelay_brain::attribution::attribute_fan_growth(&evidence);
        tracing::info!(
            total_observed_fans = attr.total_observed_fans,
            total_incremental_fans = attr.total_incremental_fans,
            total_durable_fans = attr.total_durable_fans,
            resolved_observations = attr.resolved_observations,
            partial_observations = attr.partial_observations,
            template_count = attr.by_template.len(),
            strategy_count = attr.by_strategy.len(),
            "fan-growth attribution summary (full replay)"
        );
        return Ok((model, evidence_replayed));
    }

    // Fall back to the legacy brain_evidence view for backward compatibility.
    let pool = &repo.pool;
    type EvidenceRow = (
        String,
        f64,
        f64,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        serde_json::Value,
    );
    let rows: Vec<EvidenceRow> = sqlx::query_as(
        r#"
        SELECT template_id,
               expected_new_fans,
               expected_signal_installs,
               observed_new_fans,
               observed_incremental_fans,
               observed_signal_installs,
               context
        FROM viryaos_brain_evidence
        WHERE workspace_id = $1
          AND resolved_at IS NOT NULL
          AND (observed_new_fans IS NOT NULL
               OR observed_incremental_fans IS NOT NULL
               OR observed_signal_installs IS NOT NULL)
        ORDER BY predicted_at ASC
        LIMIT 5000
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    let legacy_count = u32::try_from(rows.len()).unwrap_or(u32::MAX);
    let mut model = CausalModel::default();
    for (
        template_id,
        expected_fans,
        expected_signal,
        observed_fans,
        observed_incremental_fans,
        observed_signal,
        context_json,
    ) in rows
    {
        let context: crowdrelay_brain::DispatchContext =
            serde_json::from_value(context_json).unwrap_or_default();
        let prediction = DispatchPrediction {
            template_id: template_id.clone(),
            expected_new_fans: expected_fans,
            expected_signal_installs: expected_signal,
            context,
            // The legacy `viryaos_brain_evidence` view has no target
            // column. This path is the pre-evidence-table fallback, so it
            // teaches the template and audience type only.
            target_key: None,
            creative_family: None,
        };
        // The outcome model learns P(Y|action,context) from raw observed
        // fan counts, not from DiD estimates. Prefer observed_fans (raw)
        // and fall back to observed_incremental_fans only for legacy rows
        // that don't have the raw count populated.
        let outcome_fans = observed_fans.or(observed_incremental_fans).unwrap_or(0.0);
        let outcome = PredictionOutcome::from_observation(
            prediction,
            outcome_fans,
            observed_signal.unwrap_or(0.0),
        );
        model.update(&outcome);
    }

    Ok((model, legacy_count))
}

/// Loads the exploration memory from past dispatch predictions. Each
/// prediction is a "visit" to a (template, context) pair. The brain uses
/// this to compute novelty: unexplored pairs get an exploration bonus.
///
/// The context hash is derived from the prediction's context fields, so
/// two dispatches with the same context features count as the same visit.
///
/// We load the full context jsonb and deserialize it into `DispatchContext`
/// so the hash matches what was stored at prediction time. Previously, only
/// a subset of fields was loaded, causing a hash mismatch that made every
/// context appear novel (novelty always 1.0).
pub(in crate::autopilot) async fn load_exploration_memory(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<crowdrelay_brain::ExplorationMemory, RepositoryError> {
    use crowdrelay_brain::{DispatchContext, ExplorationMemory, VISIT_DECAY, context_hash};
    use time::OffsetDateTime;

    /// Exploration row: (template_id, context_json, predicted_at)
    type ExplorationRow = (String, serde_json::Value, OffsetDateTime);
    let pool = &repo.pool;
    let now = OffsetDateTime::now_utc();
    let rows: Vec<ExplorationRow> = sqlx::query_as(
        r#"
            SELECT template_id,
                   context,
                   predicted_at
            FROM viryaos_dispatch_predictions
            WHERE workspace_id = $1
              AND predicted_at >= now() - INTERVAL '12 hours'
            ORDER BY predicted_at DESC
            LIMIT 500
            "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    let mut mem = ExplorationMemory::default();
    // The autopilot cycle runs every 5 minutes. Each historical visit is
    // weighted by VISIT_DECAY^age_cycles so old visits contribute less.
    // Use fractional hours (not whole_hours) so sub-hour visits decay
    // correctly — with a 5-minute cycle, the first 12 cycles all had
    // age_hours=0 and full weight when using whole_hours.
    const CYCLE_HOURS: f64 = 5.0 / 60.0; // 5 minutes in hours
    for (template_id, context_json, predicted_at) in rows {
        let age_hours = (now - predicted_at).as_seconds_f64() / 3600.0;
        let age_cycles = age_hours / CYCLE_HOURS;
        let decayed_weight = VISIT_DECAY.powf(age_cycles);
        // Skip visits that have decayed to near-zero.
        if decayed_weight < 0.01 {
            continue;
        }
        let ctx: DispatchContext = serde_json::from_value(context_json).unwrap_or_default();
        let hash = context_hash(&ctx);
        mem.record_decayed_visit(&template_id, &hash, decayed_weight);
    }
    Ok(mem)
}

/// Loads the most recently dispatched template's ID. Used to infer the
/// previous growth strategy for hysteresis — the brain doesn't flip-flop
/// between strategies every cycle when conditions are borderline.
pub(in crate::autopilot) async fn load_last_dispatched_template(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<Option<String>, RepositoryError> {
    let pool = &repo.pool;
    let template: Option<String> = sqlx::query_scalar(
        r#"
        SELECT template_id
        FROM viryaos_dispatch_predictions
        WHERE workspace_id = $1
        ORDER BY predicted_at DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(template)
}

/// Loads hypothesis lifecycle states for all templates in a workspace.
///
/// Returns a map from template_id to HypothesisState. Templates not in
/// the table default to Active (preserving current behavior for existing
/// templates that haven't been persisted yet).
pub(in crate::autopilot) async fn load_hypothesis_states(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<
    std::collections::HashMap<String, crowdrelay_brain::hypothesis::HypothesisState>,
    RepositoryError,
> {
    let pool = &repo.pool;
    let rows: Vec<(String, String)> = sqlx::query_as(
        r#"
        SELECT template_id, state
        FROM viryaos_growth_hypotheses
        WHERE workspace_id = $1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;
    let mut states = std::collections::HashMap::new();
    for (template_id, state_str) in rows {
        if let Some(state) = crowdrelay_brain::hypothesis::HypothesisState::parse(&state_str) {
            states.insert(template_id, state);
        }
    }
    Ok(states)
}

/// Saves a hypothesis lifecycle state, creating or updating the row.
pub(in crate::autopilot) async fn save_hypothesis_state(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    template_id: &str,
    state: crowdrelay_brain::hypothesis::HypothesisState,
) -> Result<(), RepositoryError> {
    let pool = &repo.pool;
    sqlx::query(
        r#"
        INSERT INTO viryaos_growth_hypotheses (
            workspace_id, template_id, state, last_transition_at, updated_at
        ) VALUES ($1, $2, $3, now(), now())
        ON CONFLICT (workspace_id, template_id)
            DO UPDATE SET state = $3,
                          last_transition_at = CASE
                              WHEN viryaos_growth_hypotheses.state != $3
                              THEN now()
                              ELSE viryaos_growth_hypotheses.last_transition_at
                          END,
                          updated_at = now()
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(template_id)
    .bind(state.as_str())
    .execute(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}
