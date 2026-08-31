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
use crowdrelay_brain::{
    CommunityEngagementSummary, GrowthIntelligenceSnapshot, GrowthTarget, GrowthTargetProgress,
    GrowthTrend, RecentInsight, TenantPreferencePosterior, UnengagedTarget, WorldModel,
    agent_standing_policy,
};
use crowdrelay_domain::growth_metrics::NorthStarMetric;
use crowdrelay_domain::learning::{OutcomeRecord, Standing, assess_standing};

/// The worker templates the brain may dispatch, in the order the evaluator
/// checks them. Adding a new worker template means adding it here and to the
/// evaluator's rules.
const WORKER_TEMPLATES: &[&str] = &[
    "reddit-scanner",
    "telegram-scanner",
    "metal-archives-scanner",
    "bandcamp-scanner",
    "press-pitch",
    "social-post",
    "telegram-poster",
    "community-engager",
    "signal-inviter",
    "growth-strategist",
];

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
    let last_runs: Vec<(String, Option<OffsetDateTime>, Option<OffsetDateTime>)> = sqlx::query_as(
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
    .map_err(map_sqlx)?;

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

    // Load the actual promoted community targets with a subreddit so the
    // community-engager prompt can include concrete target_id + subreddit
    // pairs. The LLM needs these to produce social_post outcomes that
    // result in community.engage.request actions. The count of promoted
    // targets is derived from this query's row count, so we don't need a
    // separate count query.
    let unengaged_target_rows: Vec<(uuid::Uuid, String, String)> = sqlx::query_as(
        r#"
        SELECT id, display_name, subreddit
        FROM agent_outreach_targets
        WHERE workspace_id = $1
          AND status = 'promoted'
          AND target_kind = 'community'
          AND subreddit IS NOT NULL
        ORDER BY created_at DESC
        LIMIT 20
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    let unengaged_targets: Vec<UnengagedTarget> = unengaged_target_rows
        .into_iter()
        .map(|(target_id, display_name, subreddit)| UnengagedTarget {
            target_id,
            display_name,
            subreddit,
        })
        .collect();

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
    // We join with agent_service_tasks to get the template_id that produced
    // each insight, so the brain can attach insights to the right snapshot.
    let insights: Vec<(uuid::Uuid, String, String, String, String, Option<String>)> =
        sqlx::query_as(
            r#"
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
            LIMIT 50
            "#,
        )
        .bind(workspace_id.into_uuid())
        .fetch_all(pool)
        .await
        .map_err(map_sqlx)?;

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

    // Load agent standings from past measurement outcomes. The brain learns
    // which worker templates produce fan growth and which don't, and adjusts
    // dispatch cadence accordingly. We load raw outcomes ordered by
    // observed_at DESC and compute the OutcomeRecord (with
    // consecutive_worsened) in Rust — simpler and more testable than a
    // window-function SQL approach.
    let standing_rows: Vec<(String, String, OffsetDateTime)> = sqlx::query_as(
        r#"
        SELECT action.payload->>'template_id' AS template_id,
               outcome.effect_assessment,
               outcome.observed_at
        FROM viryaos_autopilot_outcomes outcome
        JOIN viryaos_autopilot_actions action ON action.id = outcome.action_id
        WHERE action.workspace_id = $1
          AND action.action_kind = 'agent.run.request'
          AND outcome.effect_assessment IS NOT NULL
        ORDER BY action.payload->>'template_id', outcome.observed_at DESC
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    // Build OutcomeRecord per template by iterating the ordered rows.
    let policy = agent_standing_policy();

    // Load operator feedback (approve/cancel verdicts) from operator_actions.
    // This is the execution-quality signal: "would a human operator accept
    // this draft?" — separate from the causal fan-growth signal above. We
    // join operator_actions with autopilot_actions to get the template_id
    // and time-to-decision (operator_action.created_at - action.created_at).
    // Fast = within 1 hour (configurable via GrowthIntelligencePolicy).
    let operator_feedback_rows: Vec<(String, String, f64)> = sqlx::query_as(
        r#"
        SELECT action.payload->>'template_id' AS template_id,
               oa.action AS operator_action,
               EXTRACT(EPOCH FROM (oa.created_at - action.created_at)) / 3600.0 AS hours_to_decision
        FROM operator_actions oa
        JOIN viryaos_autopilot_actions action ON action.id = oa.target_id
        WHERE action.workspace_id = $1
          AND action.action_kind = 'agent.run.request'
          AND oa.target_type = 'autopilot_action'
          AND oa.action IN ('approve_autopilot_action', 'cancel_autopilot_action')
        ORDER BY action.payload->>'template_id', oa.created_at DESC
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    let operator_fast_threshold_hours = 1.0_f64;

    let standings: std::collections::HashMap<String, Standing> = {
        let mut records: std::collections::HashMap<String, OutcomeRecord> =
            std::collections::HashMap::new();
        for (template_id, assessment, _observed_at) in &standing_rows {
            let record = records.entry(template_id.clone()).or_default();
            record.improved += u32::from(assessment == "improved");
            record.neutral += u32::from(assessment == "neutral");
            record.worsened += u32::from(assessment == "worsened");
            // consecutive_worsened: count from the most recent (first in the
            // DESC-ordered rows) until we hit a non-worsened outcome.
            if assessment == "worsened" {
                // Only increment if all prior rows (more recent) were also
                // worsened. We check by seeing if improved+neutral is still 0.
                if record.improved == 0 && record.neutral == 0 {
                    record.consecutive_worsened += 1;
                }
            }
            // else: the streak is broken — but we've already counted the
            // improved/neutral, so the condition above naturally stops
            // incrementing consecutive_worsened for any older worsened rows.
        }
        // Fold operator feedback into the records. Operator feedback does NOT
        // update consecutive_worsened — only measured fan-growth outcomes can
        // trigger retirement. Operator cancellations affect the weight (and
        // thus cooldown) but cannot retire a worker on their own.
        for (template_id, operator_action, hours_to_decision) in &operator_feedback_rows {
            let record = records.entry(template_id.clone()).or_default();
            let approved = operator_action == "approve_autopilot_action";
            let fast = *hours_to_decision <= operator_fast_threshold_hours;
            *record = (*record).observe_operator(approved, fast);
        }
        records
            .into_iter()
            .map(|(template_id, record)| (template_id, assess_standing(record, policy)))
            .collect()
    };

    // ── Tenant operating preference ──
    // Build a TenantPreferencePosterior from the same raw operator actions,
    // but interpreted as "does this tenant prefer this template?" rather than
    // "is this execution acceptable?". Uses exponentially decayed evidence
    // (90-day half-life default) so preferences can shift over time.
    //
    // This is a SEPARATE belief from Standing. Both consume the same raw
    // operator events but answer different questions:
    //   Standing        → "Is this worker performing well?" → cooldown/tier
    //   TenantPreference → "Does this tenant prefer this template?" → cadence
    //
    // The preference posterior MUST NOT modify DecisionValue or any economic
    // value. It only influences cadence timing. Presentation metadata is
    // derived brain-side but currently NOT persisted (TODO: wire to operator
    // read path when the UI supports it).
    let tenant_preference: TenantPreferencePosterior = {
        let mut posterior = TenantPreferencePosterior::new();
        // Reuse the operator_feedback_rows already loaded above, but compute
        // age-based decay instead of fast/slow classification. We need the
        // operator action timestamp for age computation — query it fresh
        // since the existing rows only carry hours_to_decision, not age.
        let preference_rows: Vec<(String, bool, f64)> = sqlx::query_as(
            r#"
            SELECT COALESCE(action.payload->>'template_id', 'unknown') AS template_id,
                   (oa.action = 'approve_autopilot_action') AS approved,
                   EXTRACT(EPOCH FROM (now() - oa.created_at)) / 86400.0 AS age_days
            FROM operator_actions oa
            JOIN viryaos_autopilot_actions action ON action.id = oa.target_id
            WHERE action.workspace_id = $1
              AND action.action_kind = 'agent.run.request'
              AND oa.target_type = 'autopilot_action'
              AND oa.action IN ('approve_autopilot_action', 'cancel_autopilot_action')
            "#,
        )
        .bind(workspace_id.into_uuid())
        .fetch_all(pool)
        .await
        .map_err(map_sqlx)?;
        // The half-life matches TenantPreferencePolicy::default().half_life_days
        // (90 days). The infra layer doesn't receive the GrowthIntelligencePolicy
        // (it's loaded in the application layer), so we use the default here.
        // Future: wire the policy through if per-workspace customization is needed.
        let half_life = 90.0_f64;
        for (template_id, approved, age_days) in &preference_rows {
            posterior.observe(template_id, *approved, *age_days, half_life);
        }
        posterior
    };

    // ── World Model data ──
    // The brain's belief about the world: fan counts, signal installs,
    // community reach, outreach pipeline, and growth target progress.
    // Loaded once and shared across all template snapshots. The recent_fans
    // count (last 14 days) is merged into this query to save a round-trip.
    let fan_counts: (i64, i64, i64, i64) = sqlx::query_as(
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
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)?;

    let total_fans = u32::try_from(fan_counts.0.max(0)).unwrap_or(0);
    let fans_this_month = u32::try_from(fan_counts.1.max(0)).unwrap_or(0);
    let fans_prev_window = u32::try_from(fan_counts.2.max(0)).unwrap_or(0);
    let fan_growth_stagnant = fan_counts.3 == 0;

    // Signal install counts.
    let signal_counts: (i64, i64) = sqlx::query_as(
        r#"
        SELECT
            COUNT(*)::bigint AS total_installs,
            COUNT(*) FILTER (WHERE created_at > date_trunc('month', now()))::bigint AS installs_this_month
        FROM fan_push_endpoints
        WHERE workspace_id = $1 AND active = true AND invalidated_at IS NULL
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)?;

    let total_signal_installs = u32::try_from(signal_counts.0.max(0)).unwrap_or(0);
    let signal_installs_this_month = u32::try_from(signal_counts.1.max(0)).unwrap_or(0);

    // Discovered communities.
    let community_counts: (i64, i64) = sqlx::query_as(
        r#"
        SELECT
            COUNT(*)::bigint AS discovered,
            COUNT(DISTINCT cp.subreddit)::bigint AS active
        FROM discovery_places dp
        LEFT JOIN community_posts cp ON cp.subreddit = dp.name
            AND cp.workspace_id = dp.workspace_id
            AND cp.posted_at > now() - interval '30 days'
        WHERE dp.workspace_id = $1 AND dp.status = 'active'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)?;

    let discovered_communities = u32::try_from(community_counts.0.max(0)).unwrap_or(0);
    let active_communities = u32::try_from(community_counts.1.max(0)).unwrap_or(0);

    // Outreach pipeline counts by status.
    let outreach_counts: (i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT
            COUNT(*) FILTER (WHERE ot.status = 'proposed')::bigint AS pending,
            COUNT(*) FILTER (WHERE ot.status = 'promoted')::bigint AS promoted,
            COUNT(DISTINCT cp.target_id)::bigint AS engaged
        FROM agent_outreach_targets ot
        LEFT JOIN community_posts cp ON cp.target_id = ot.id
            AND cp.workspace_id = ot.workspace_id
            AND cp.status = 'posted'
        WHERE ot.workspace_id = $1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(pool)
    .await
    .map_err(map_sqlx)?;

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

    // Load the north star metric from tenant_settings. Default is signal_installs.
    let north_star_str: Option<(String,)> = sqlx::query_as(
        r#"SELECT value FROM tenant_settings WHERE workspace_id = $1 AND key = 'north_star_metric'"#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx)?;
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
            // SignalInstalls, and any future north star without a platform series.
            _ => (total_signal_installs, signal_installs_this_month),
        };

    // Growth target progress.
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

    // Build one snapshot per worker template.
    let mut snapshots = Vec::with_capacity(WORKER_TEMPLATES.len());
    for template_id in WORKER_TEMPLATES {
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

        // Attach insights produced by this template.
        let template_insights: Vec<RecentInsight> = recent_insights
            .iter()
            .filter(|i| i.template_id == *template_id)
            .cloned()
            .collect();

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
        });
    }

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
) -> Result<crowdrelay_brain::CausalModel, RepositoryError> {
    use crowdrelay_brain::CausalModel;

    // Try to load a brain state checkpoint for fast startup.
    let checkpoint = super::evidence::load_brain_state(repo, workspace_id, "causal_model").await?;
    let model = if let Some((state_json, checkpoint_time)) = checkpoint {
        match serde_json::from_value::<CausalModel>(state_json) {
            Ok(mut model) => {
                // Load only delta evidence since the checkpoint.
                let delta = super::evidence::load_growth_evidence(
                    repo,
                    workspace_id,
                    Some(checkpoint_time),
                )
                .await?;
                apply_evidence_to_model(&mut model, &delta);
                // Also apply delta evidence to the strategy posterior so it
                // stays in sync with the causal model's evidence replay.
                apply_delta_to_strategy_posterior(repo, workspace_id, &delta).await;
                tracing::debug!(
                    delta_evidence = delta.len(),
                    "loaded causal model from checkpoint + delta"
                );
                model
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to deserialize brain checkpoint, falling back to full replay");
                full_replay(repo, workspace_id).await?
            }
        }
    } else {
        // No checkpoint — full replay from evidence table or legacy view.
        full_replay(repo, workspace_id).await?
    };

    Ok(model)
}

/// Applies a batch of growth evidence to the causal model, updating the
/// outcome model, context effects, regime-isolated calibration, the
/// Y14/Y30 treatment-effect posteriors, and the Y14→Y30 bridge.
///
/// CALIBRATION REGIME ISOLATION: Y14 treatment-effect observations are
/// recorded to the Y14Bridged regime tracker, Y30 treatment-effect
/// observations to the Y30Direct regime tracker, and outcome model
/// observations to the OutcomeModel regime tracker. This ensures that
/// a badly calibrated observational predictor cannot distort uncertainty
/// for the randomized treatment estimator.
fn apply_evidence_to_model(
    model: &mut crowdrelay_brain::CausalModel,
    evidence: &[crowdrelay_brain::GrowthEvidence],
) {
    use crowdrelay_brain::{
        CausalEstimand, DispatchPrediction, EstimationRegime, ExecutionStatus, PredictionOutcome,
    };

    // The active causal estimand. This is the explicit domain decision
    // that determines which evidence rows contribute to the treatment-
    // effect posterior. SQL provides eligible observations; the causal
    // layer chooses the estimand. Do NOT let naming outrun identification.
    //
    // ITT is the safest default: it includes all assigned units, uses the
    // arm (Z) as the treatment indicator, and does not exclude based on
    // execution_status. This avoids the semantic shortcut of equating
    // `execution_status = executed` with "TOT is identified".
    let estimand = CausalEstimand::IntentToTreat;

    for ev in evidence {
        let template = extract_template_from_opportunity(&ev.opportunity_id);
        let subreddit_type = ev.context.subreddit_type.as_deref();
        // Scale the observation variance by the evidence quality multiplier.
        // Higher quality evidence (randomized holdout) gets a lower variance
        // → moves the posterior more. Lower quality evidence (observational)
        // gets a higher variance → barely moves the posterior. This prevents
        // weak pre/post evidence from dominating strong causal evidence.
        let quality_multiplier = ev.evidence_quality.variance_multiplier();

        // Update the outcome model (P(Y|action,context)) from the raw
        // observed fan count — NOT the DiD estimate. The outcome model
        // learns the expected raw fan count given an action and context.
        // The treatment-effect posterior (updated below) learns from the
        // counterfactual-adjusted DiD estimate. These are separate learning
        // targets and must not be conflated.
        //
        // The outcome model is ALWAYS updated from all rows regardless of
        // estimand — it learns the raw expected fan count, which is
        // estimand-agnostic. The estimand only gates the treatment-effect
        // posterior.
        //
        // We use `observed_fans` (raw count) when available. If only the
        // incremental estimate is available (legacy evidence rows), we
        // skip the outcome model update rather than feeding it a DiD
        // estimate that would be clamped to 0 on negative values.
        if let Some(raw_fans) = ev.observed_fans {
            let prediction = DispatchPrediction {
                template_id: template.clone(),
                expected_new_fans: ev.predicted_fans,
                expected_signal_installs: ev.predicted_signal_installs,
                context: ev.context.clone(),
            };
            let outcome = PredictionOutcome::from_observation(prediction, raw_fans, 0.0);
            model.update(&outcome);
        }

        // Determine whether this evidence row contributes to the
        // treatment-effect posterior under the active estimand.
        //
        // The estimand's `includes_in_treatment_effect` method is the
        // ONLY place that decides this — not SQL, not ad-hoc filtering.
        // If execution_status is missing (legacy rows), default to
        // Executed for treatment arm and Control for control arm, which
        // preserves the old behavior under ITT.
        let execution_status = ev
            .execution_status
            .unwrap_or(if ev.treatment.is_treatment() {
                ExecutionStatus::Executed
            } else {
                ExecutionStatus::Control
            });
        let contributes_to_tau =
            estimand.includes_in_treatment_effect(ev.treatment.is_treatment(), execution_status);

        if !contributes_to_tau {
            continue;
        }

        // Update the Y14 treatment-effect posterior from the incremental
        // outcome. The `observed_incremental_fans` field is the
        // counterfactual-adjusted τ estimate (already IPW-corrected if
        // propensity is available). The observation variance is scaled by
        // the evidence quality — weak evidence barely moves the posterior.
        //
        // Y14 treatment-effect calibration is recorded to the Y14Bridged
        // regime tracker — separate from Y30Direct and OutcomeModel.
        if let Some(tau_y14) = ev.observed_incremental_fans {
            let obs_var = 2.0 * tau_y14.abs().max(1.0) * quality_multiplier;
            model.update_treatment_effect(&template, subreddit_type, tau_y14, obs_var);
            // Record Y14Bridged calibration with the actual measurement-
            // determined evidence quality, not a synthesized one.
            model.calibration.record_by_regime(
                EstimationRegime::Y14Bridged,
                &template,
                ev.predicted_fans,
                2.0,
                tau_y14,
                subreddit_type,
                None,
                ev.evidence_quality.as_str(),
            );
        }

        // When Y30 (durable) is available, update the Y30 treatment-effect
        // posterior, the Y30Direct calibration tracker, and the Y14→Y30
        // bridge.
        if let Some(y30_fans) = ev.y30_outcome() {
            // Y30 treatment-effect update (North Star). Scaled by evidence
            // quality — same rationale as Y14.
            let obs_var = 2.0 * y30_fans.abs().max(1.0) * quality_multiplier;
            model.update_treatment_effect_y30(&template, subreddit_type, y30_fans, obs_var);
            // Y30Direct calibration — isolated from Y14Bridged and
            // OutcomeModel. A bad OutcomeModel calibration cannot distort
            // Y30Direct uncertainty. Uses the actual measurement-determined
            // evidence quality.
            model.calibration.record_by_regime(
                EstimationRegime::Y30Direct,
                &template,
                ev.predicted_fans,
                2.0,
                y30_fans,
                subreddit_type,
                None,
                ev.evidence_quality.as_str(),
            );
            // Y14→Y30 bridge: update when both outcomes are available.
            if let Some(y14_fans) = ev.y14_outcome() {
                model.update_bridge(y14_fans, y30_fans);
            }
        }
    }
}

/// Records strategy outcomes into the state-conditioned strategy posterior
/// from growth evidence. The strategy is inferred from the evidence's
/// template_id, and the state (growth_trend, event_proximity) comes from
/// the evidence's context. This is called alongside `apply_evidence_to_model`
/// during the causal model load, so the strategy posterior stays in sync
/// with the causal model's evidence replay.
pub(in crate::autopilot) fn apply_evidence_to_strategy_posterior(
    posterior: &mut crowdrelay_brain::StateConditionedStrategyPosterior,
    evidence: &[crowdrelay_brain::GrowthEvidence],
) {
    use crowdrelay_brain::GrowthStrategy;

    for ev in evidence {
        // Use the recorded strategy when available. Legacy evidence rows
        // (before the strategy field was added) fall back to inferring
        // from the template — a heuristic guess that can be wrong when
        // multiple strategies dispatch the same template.
        let template = extract_template_from_opportunity(&ev.opportunity_id);
        let strategy = ev
            .strategy
            .as_deref()
            .map(|s| s.to_owned())
            .unwrap_or_else(|| {
                GrowthStrategy::infer_from_template(&template)
                    .as_str()
                    .to_owned()
            });
        let growth_trend = ev.context.fan_growth_trend.as_str();
        let event_proximity = match ev.context.days_to_event {
            Some(d) if d <= 7 => "close",
            Some(d) if d <= 30 => "near",
            _ => "far",
        };
        // Use the Y14 incremental outcome as the strategy effectiveness
        // signal. This is the counterfactual-adjusted estimate of how many
        // fans the dispatch produced — exactly what we want to learn which
        // strategies work best.
        if let Some(incremental_fans) = ev.observed_incremental_fans {
            let obs_var = 2.0 * incremental_fans.abs().max(1.0);
            posterior.update(
                &strategy,
                growth_trend,
                event_proximity,
                incremental_fans,
                obs_var,
            );
        }
    }
}

/// Loads the strategy posterior from brain state, applies delta evidence to
/// it, and saves it back. Called alongside `apply_evidence_to_model` during
/// the causal model load so the strategy posterior stays in sync with the
/// causal model's evidence replay.
async fn apply_delta_to_strategy_posterior(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    delta: &[crowdrelay_brain::GrowthEvidence],
) {
    use crowdrelay_brain::StateConditionedStrategyPosterior;

    // Load the existing strategy posterior from brain state.
    let posterior =
        match super::evidence::load_brain_state(repo, workspace_id, "strategy_posterior").await {
            Ok(Some((state, _ts))) => {
                serde_json::from_value::<StateConditionedStrategyPosterior>(state)
                    .unwrap_or_default()
            }
            _ => StateConditionedStrategyPosterior::default(),
        };

    let mut posterior = posterior;
    apply_evidence_to_strategy_posterior(&mut posterior, delta);

    // Save the updated posterior back to brain state. Best-effort.
    if let Ok(state) = serde_json::to_value(&posterior) {
        let _ = super::evidence::save_brain_state(repo, workspace_id, "strategy_posterior", &state)
            .await;
    }
}

/// Extracts the template_id from an opportunity ID string.
/// Opportunity IDs are formatted as "template:target:action:context_hash".
fn extract_template_from_opportunity(opportunity_id: &Option<String>) -> String {
    opportunity_id
        .as_ref()
        .and_then(|s| s.split(':').next())
        .unwrap_or("unknown")
        .to_owned()
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
async fn full_replay(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<crowdrelay_brain::CausalModel, RepositoryError> {
    use crowdrelay_brain::{CausalModel, DispatchPrediction, PredictionOutcome};

    // Try the new growth evidence table first.
    let evidence = super::evidence::load_growth_evidence(repo, workspace_id, None).await?;
    if !evidence.is_empty() {
        let mut model = CausalModel::default();
        apply_evidence_to_model(&mut model, &evidence);
        // Also replay evidence into the strategy posterior from scratch.
        apply_delta_to_strategy_posterior(repo, workspace_id, &evidence).await;
        return Ok(model);
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
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

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

    Ok(model)
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

#[cfg(test)]
mod tests {
    use super::apply_evidence_to_model;
    use crowdrelay_brain::{CausalModel, DispatchContext, GrowthEvidence, TreatmentAssignment};

    /// Brain-level evidence-eligibility invariant:
    ///
    /// Evidence with a treatment assignment but NO observed outcome
    /// (`observed_incremental_fans = None`) must NOT move the
    /// treatment-effect posterior. The `apply_evidence_to_model`
    /// function guards `update_treatment_effect` behind
    /// `if let Some(tau_y14) = ev.observed_incremental_fans`, so
    /// absent outcomes are naturally skipped. This test proves the
    /// guard works by constructing real evidence and passing it
    /// through the actual evidence-processing path.
    ///
    /// This is the brain-level complement to T25i (which proves the
    /// SQL boundary excludes UNKNOWN evidence). Together they form
    /// two independent defenses:
    /// - T25i → SQL/persistence learning-boundary proof
    /// - This test → model-level evidence-eligibility proof
    #[test]
    fn evidence_without_observed_outcome_does_not_update_treatment_posterior() {
        let mut model = CausalModel::new();
        let ctx = DispatchContext::default();
        let template = "community.engage";
        let before = model.predict_stats_with_treatment(template, &ctx);

        // Construct real evidence with treatment assignment but no
        // observed outcome. This is what an unresolved/UNKNOWN dispatch
        // looks like if it somehow reached the learner.
        let evidence = GrowthEvidence {
            opportunity_id: Some(format!("{template}:subreddit:community.engage.request:ctx")),
            treatment: TreatmentAssignment::Treatment,
            observed_incremental_fans: None, // ← no outcome
            observed_fans: None,             // ← no raw outcome either
            ..GrowthEvidence::default()
        };

        // Pass through the real evidence-processing path.
        apply_evidence_to_model(&mut model, &[evidence]);

        let after = model.predict_stats_with_treatment(template, &ctx);
        assert_eq!(
            before.treatment_effect, after.treatment_effect,
            "treatment posterior must not move when observed outcome is absent"
        );
        assert_eq!(
            before.treatment_confidence, after.treatment_confidence,
            "treatment confidence must not change when observed outcome is absent"
        );
        assert_eq!(
            before.use_treatment_effect, after.use_treatment_effect,
            "treatment activation must not change when observed outcome is absent"
        );
    }

    /// Positive control: evidence WITH an observed outcome DOES move
    /// the treatment-effect posterior. This proves the evidence path
    /// is actually exercised — without this test, the negative test
    /// above could be vacuously true because `apply_evidence_to_model`
    /// does nothing at all.
    #[test]
    fn evidence_with_observed_outcome_updates_treatment_posterior() {
        let mut model = CausalModel::new();
        let ctx = DispatchContext::default();
        let template = "community.engage";
        let before = model.predict_stats_with_treatment(template, &ctx);

        // Construct evidence with a real observed outcome.
        let evidence = GrowthEvidence {
            opportunity_id: Some(format!("{template}:subreddit:community.engage.request:ctx")),
            treatment: TreatmentAssignment::Treatment,
            observed_incremental_fans: Some(5.0), // ← real outcome
            observed_fans: Some(10.0),            // ← raw outcome
            predicted_fans: 3.0,
            ..GrowthEvidence::default()
        };

        apply_evidence_to_model(&mut model, &[evidence]);

        let after = model.predict_stats_with_treatment(template, &ctx);
        // The treatment effect estimate should have moved from the
        // prior (0.0) toward the observed value (5.0).
        assert_ne!(
            before.treatment_effect, after.treatment_effect,
            "treatment posterior must move when an observed outcome is present"
        );
    }
}
