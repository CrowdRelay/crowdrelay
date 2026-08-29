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
//! 2. **Causal Model** — P(new_fan | template, context) with EMA learning.
//!    The brain predicts before dispatch and learns from prediction error
//!    after measurement (the dopamine loop).
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
    GrowthTrend, RecentInsight, UnengagedTarget, WorldModel, agent_standing_policy,
};
use crowdrelay_domain::learning::{OutcomeRecord, Standing, assess_standing};

/// The worker templates the brain may dispatch, in the order the evaluator
/// checks them. Adding a new worker template means adding it here and to the
/// evaluator's rules.
const WORKER_TEMPLATES: &[&str] = &[
    "reddit-scanner",
    "press-pitch",
    "social-post",
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
        records
            .into_iter()
            .map(|(template_id, record)| (template_id, assess_standing(record, policy)))
            .collect()
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
            COUNT(*) FILTER (WHERE status = 'proposed')::bigint AS pending,
            COUNT(*) FILTER (WHERE status = 'promoted')::bigint AS promoted,
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

    // Growth target progress.
    let growth_target = GrowthTarget::from_fan_count(total_fans);
    let growth_target_progress = GrowthTargetProgress::from_counts(
        growth_target,
        fans_this_month,
        signal_installs_this_month,
    );

    let world_model = WorldModel {
        total_fans,
        fans_this_month,
        fan_growth_rate_bps,
        fan_growth_trend,
        total_signal_installs,
        signal_installs_this_month,
        signal_conversion_rate_bps,
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
/// dispatch will produce. The model is recomputed from the prediction
/// history each cycle — no separate storage needed.
///
/// In addition to the outcome model P(Y|action,context), this function also
/// loads treatment-effect observations and updates the treatment-effect
/// posterior P(τ|context). When enough paired experiment data has
/// accumulated, the brain uses τ as the primary ranking signal.
pub(in crate::autopilot) async fn load_causal_model(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<crowdrelay_brain::CausalModel, RepositoryError> {
    use crowdrelay_brain::{CausalModel, DispatchPrediction, PredictionOutcome};

    let pool = &repo.pool;

    // Load unified evidence (predictions joined with observed outcomes) from
    // the brain evidence view. This closes the learning loop: the
    // measurement system writes outcomes to viryaos_autopilot_outcomes, and
    // this view joins them back to the predictions by action_id. Previously,
    // the brain read from raw viryaos_dispatch_predictions which never had
    // observed_new_fans / resolved_at populated — so the causal model
    // learned from an empty dataset every cycle.
    //
    // We load the full context jsonb so the hierarchical model can learn
    // per-subreddit-type effects. Rows are ordered oldest-first so the
    // EMA update processes them in chronological order.
    //
    // We prefer `observed_incremental_fans` (counterfactual-adjusted) over
    // `observed_new_fans` (raw count) when available — this is the causally
    // correct outcome that isolates the dispatch's effect from organic
    // growth.
    /// Evidence row: (template_id, expected_fans, expected_signal, observed_fans, observed_incremental_fans, observed_signal, context_json)
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
        // Prefer incremental fan growth (counterfactual-adjusted) when
        // available — this is the causally correct outcome. Fall back to
        // raw observed fans for backward compatibility.
        let outcome_fans = observed_incremental_fans.or(observed_fans).unwrap_or(0.0);
        let outcome = PredictionOutcome::from_observation(
            prediction,
            outcome_fans,
            observed_signal.unwrap_or(0.0),
        );
        model.update(&outcome);
    }

    // Load treatment-effect observations and update the treatment-effect
    // posterior. Each row is a pre-computed τ estimate from paired
    // treatment/control experiments.
    type TreatmentEffectRow = (String, Option<String>, f64, f64);
    let te_rows: Vec<TreatmentEffectRow> = sqlx::query_as(
        r#"
        SELECT template_id,
               subreddit_type,
               observed_tau,
               observation_variance
        FROM viryaos_treatment_effect_observations
        WHERE workspace_id = $1
        ORDER BY computed_at ASC
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    for (template_id, subreddit_type, observed_tau, observation_variance) in te_rows {
        model.update_treatment_effect(
            &template_id,
            subreddit_type.as_deref(),
            observed_tau,
            observation_variance,
        );
    }

    Ok(model)
}

/// Records a dispatch prediction. Called when the brain dispatches a worker
/// — stores the prediction so it can be compared with the measured outcome
/// later (after the measurement window elapses).
pub(in crate::autopilot) async fn record_dispatch_prediction(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    action_id: uuid::Uuid,
    prediction: &crowdrelay_brain::DispatchPrediction,
) -> Result<(), RepositoryError> {
    let pool = &repo.pool;
    let context_json = serde_json::to_value(&prediction.context).unwrap_or(serde_json::json!({}));
    sqlx::query(
        r#"
        INSERT INTO viryaos_dispatch_predictions
            (workspace_id, action_id, template_id,
             expected_new_fans, expected_signal_installs, context)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (action_id) DO NOTHING
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(&prediction.template_id)
    .bind(prediction.expected_new_fans)
    .bind(prediction.expected_signal_installs)
    .bind(&context_json)
    .execute(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(())
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
            "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    let mut mem = ExplorationMemory::default();
    // The autopilot cycle runs every 5 minutes. Each historical visit is
    // weighted by VISIT_DECAY^age_cycles so old visits contribute less.
    const CYCLE_HOURS: f64 = 5.0 / 60.0; // 5 minutes in hours
    for (template_id, context_json, predicted_at) in rows {
        let age_hours = (now - predicted_at).whole_hours().max(0) as f64;
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

// ─── Brain state persistence ─────────────────────────────────────────────
//
// The brain recomputes most state from evidence each cycle (causal model,
// exploration memory). But some posteriors are expensive to recompute
// (treatment effects, strategy, overlap, calibration, fan network). These
// are stored in viryaos_brain_state as serialized jsonb and loaded on
// startup for fast decisions.
//
// The invariant: every state used to make a decision must either be
// persisted here or deterministically reconstructable from persisted
// evidence (viryaos_brain_evidence view, viryaos_treatment_effect_observations,
// viryaos_opportunity_episodes, etc.).

/// Brain state module identifiers — must match the CHECK constraint in
/// viryaos_brain_state.
#[allow(dead_code)] // Wired in Phase 0.2 and Phase 1
pub(in crate::autopilot) const BRAIN_STATE_MODULE_TREATMENT_EFFECT: &str = "treatment_effect";
#[allow(dead_code)]
pub(in crate::autopilot) const BRAIN_STATE_MODULE_STRATEGY_POSTERIOR: &str = "strategy_posterior";
#[allow(dead_code)]
pub(in crate::autopilot) const BRAIN_STATE_MODULE_OVERLAP_MODEL: &str = "overlap_model";
#[allow(dead_code)]
pub(in crate::autopilot) const BRAIN_STATE_MODULE_CALIBRATION: &str = "calibration";
#[allow(dead_code)]
pub(in crate::autopilot) const BRAIN_STATE_MODULE_FAN_NETWORK: &str = "fan_network";
#[allow(dead_code)]
pub(in crate::autopilot) const BRAIN_STATE_MODULE_CHANGE_POINT: &str = "change_point";
#[allow(dead_code)]
pub(in crate::autopilot) const BRAIN_STATE_MODULE_EPISODE_TRACKER: &str = "episode_tracker";

/// Loads serialized brain state for a given module. Returns None if no
/// state has been persisted yet (the brain will use its default/prior).
#[allow(dead_code)]
pub(in crate::autopilot) async fn load_brain_state(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    module: &str,
) -> Result<Option<serde_json::Value>, RepositoryError> {
    let pool = &repo.pool;
    let state: Option<(serde_json::Value,)> = sqlx::query_as(
        r#"
        SELECT state FROM viryaos_brain_state
        WHERE workspace_id = $1 AND module = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(module)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(state.map(|(s,)| s))
}

/// Saves serialized brain state for a given module. Upserts by
/// (workspace_id, module).
#[allow(dead_code)]
pub(in crate::autopilot) async fn save_brain_state(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    module: &str,
    state: &serde_json::Value,
) -> Result<(), RepositoryError> {
    let pool = &repo.pool;
    sqlx::query(
        r#"
        INSERT INTO viryaos_brain_state (workspace_id, module, state, updated_at)
        VALUES ($1, $2, $3, now())
        ON CONFLICT (workspace_id, module)
        DO UPDATE SET state = $3, updated_at = now()
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(module)
    .bind(state)
    .execute(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

/// Computes and stores treatment-effect observations from resolved
/// predictions.
///
/// For each template, we compute the average observed outcome (treatment
/// group — the template was dispatched) and compare it to the global
/// average outcome across all other templates (soft control). The
/// treatment effect τ = treatment_mean - control_mean.
///
/// This is a simplified A/B estimate that doesn't require explicit
/// treatment/control assignment. It assumes that when a template is NOT
/// dispatched, the outcome is approximately what would have happened
/// without treatment. This is valid when:
/// - Different templates are dispatched in different cycles (no perfect
///   confounding between templates)
/// - The outcome (fan growth) is measured in the same way for all
///   templates
///
/// The results are written to `viryaos_treatment_effect_observations`
/// and loaded by `load_causal_model` on the next cycle.
///
/// This function is called after measurement resolution to keep the
/// treatment-effect posterior up to date.
#[allow(dead_code)] // Wired in Phase 1.1
pub(in crate::autopilot) async fn compute_and_store_treatment_effects(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<(), RepositoryError> {
    let pool = &repo.pool;

    // Compute per-template average observed outcome and the global average.
    // We use incremental fan growth when available (counterfactual-adjusted),
    // falling back to raw fan growth.
    //
    // The query computes:
    // - For each template: avg(observed), count, variance
    // - The global average across all templates
    // - The treatment effect: avg(template) - global_avg
    // - The observation variance
    type TreatmentEffectComputation = (
        String,         // template_id
        Option<String>, // subreddit_type (extracted from context)
        f64,            // observed_tau
        f64,            // observation_variance
        i64,            // sample_size
    );
    let rows: Vec<TreatmentEffectComputation> = sqlx::query_as(
        r#"
        WITH template_stats AS (
            SELECT
                template_id,
                context ->> 'subreddit_type' AS subreddit_type,
                AVG(COALESCE(observed_incremental_fans, observed_new_fans, 0)) AS treatment_mean,
                COUNT(*) AS sample_count,
                VARIANCE(COALESCE(observed_incremental_fans, observed_new_fans, 0)) AS treatment_var
            FROM viryaos_brain_evidence
            WHERE workspace_id = $1
              AND resolved_at IS NOT NULL
              AND COALESCE(observed_incremental_fans, observed_new_fans) IS NOT NULL
            GROUP BY template_id, context ->> 'subreddit_type'
        ),
        global_stats AS (
            SELECT AVG(COALESCE(observed_incremental_fans, observed_new_fans, 0)) AS global_mean
            FROM viryaos_brain_evidence
            WHERE workspace_id = $1
              AND resolved_at IS NOT NULL
              AND COALESCE(observed_incremental_fans, observed_new_fans) IS NOT NULL
        )
        SELECT
            ts.template_id,
            ts.subreddit_type,
            ts.treatment_mean - gs.global_mean AS observed_tau,
            COALESCE(ts.treatment_var, 0.0) AS observation_variance,
            ts.sample_count
        FROM template_stats ts
        CROSS JOIN global_stats gs
        WHERE ts.sample_count >= 3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    if rows.is_empty() {
        return Ok(());
    }

    // Write the treatment-effect observations. We use upsert to update
    // existing observations when new data arrives.
    for (template_id, subreddit_type, observed_tau, observation_variance, sample_size) in rows {
        sqlx::query(
            r#"
            INSERT INTO viryaos_treatment_effect_observations
                (workspace_id, template_id, subreddit_type,
                 observed_tau, observation_variance, sample_size, computed_at)
            VALUES ($1, $2, $3, $4, $5, $6, now())
            ON CONFLICT (workspace_id, template_id, COALESCE(subreddit_type, ''))
            DO UPDATE SET
                observed_tau = $4,
                observation_variance = $5,
                sample_size = $6,
                computed_at = now()
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(&template_id)
        .bind(&subreddit_type)
        .bind(observed_tau)
        .bind(observation_variance)
        .bind(sample_size)
        .execute(pool)
        .await
        .map_err(map_sqlx)?;
    }

    Ok(())
}
