// The goal scoreboard: one read that answers "are we going to make it, and
// what is in the way" for the objective the brain is working toward.
//
// Three numbers, because a 21-day run lives or dies on them:
//
// - **planned vs actual.** What the brain's dispatches since the objective
//   was declared expected to produce (`dispatch_predictions`, the number each
//   decision was made on), beside how far the objective's own series has
//   moved. They are different units — expected Y30 fans against the series —
//   and are reported side by side, never divided into one another.
// - **learning.** Resolved evidence since declaration and in total, against
//   the 200 resolved rows the brain needs before uncertainty can honestly
//   enter selection. Pending is its own count: an unresolved row is not a
//   zero.
// - **approvals.** How long a person takes to say yes to what the brain
//   drafted. Standing grants and ladders (`operator:*`) are not people and
//   are excluded; what is still waiting is counted with its oldest age,
//   because a queue nobody drains is where a deadline goes to die.
//
// - **cut list.** Every lane (context × action kind) the brain dispatched in
//   the last 60 days, with how much of it resolved and how many fans it
//   produced. A lane with enough resolved outcomes and not one fan is a cut
//   candidate: spend the next 21 days where fans come from.
//
// - **reddit.** The account's standing as the executor reads it — open with
//   its earned daily cap, or halted and why — and the communities whose
//   moderators removed us, so the operator sees the breaker before a draft
//   hits it.
//
// With no live objective the window is the trailing 21 days, so the learning
// and approval numbers still answer before anybody declares a target.

/// Resolved evidence rows the brain needs before uncertainty can enter
/// selection (see docs/BRAIN_LEARNING_LOOP.md, known weakness 1).
const LEARNING_TARGET_RESOLVED: i64 = 200;
/// The window used when no objective is live — the run length the scoreboard
/// exists for.
const DEFAULT_GOAL_WINDOW_DAYS: i64 = 21;
/// Resolved outcomes a lane needs before "no fans" is a verdict rather than
/// an early read.
const CUT_MIN_RESOLVED: i64 = 5;

pub async fn goal(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    use crowdrelay_application::autopilot::AutopilotObjectiveRepository;

    let request_id_value = request_id(&headers);
    let workspace_id = state.ops.workspace_id();
    let now = OffsetDateTime::now_utc();
    let objective = match state.autopilot.load_active_objective(workspace_id, now).await {
        Ok(objective) => objective,
        Err(error) => {
            tracing::warn!(?error, "ops goal objective read failed");
            return Problem::service_unavailable(request_id_value)
                .private()
                .into_response();
        }
    };
    match load_goal_scoreboard(&state.ops.pool, workspace_id.into_uuid(), objective, now).await {
        Ok(scoreboard) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(scoreboard),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "ops goal scoreboard read failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

async fn load_goal_scoreboard(
    pool: &PgPool,
    workspace_id: Uuid,
    objective: Option<crowdrelay_application::ActiveObjective>,
    now: OffsetDateTime,
) -> Result<serde_json::Value, sqlx::Error> {
    let since = objective.as_ref().map_or_else(
        || now - time::Duration::days(DEFAULT_GOAL_WINDOW_DAYS),
        |objective| objective.declared_at,
    );
    let pace = objective
        .as_ref()
        .and_then(|objective| crowdrelay_application::GoalPace::from_objective(objective, now));
    // Oriented so positive is toward the target, like the assessment.
    let travelled = objective.as_ref().and_then(|objective| {
        objective.observed_value.map(|observed| {
            objective
                .direction
                .orient(observed.saturating_sub(objective.baseline_value))
        })
    });

    let (dispatches, expected_new_fans) = sqlx::query_as::<_, (i64, f64)>(
        r#"
        SELECT count(*)::bigint, COALESCE(sum(expected_new_fans), 0)::double precision
        FROM dispatch_predictions
        WHERE workspace_id = $1 AND predicted_at >= $2
        "#,
    )
    .bind(workspace_id)
    .bind(since)
    .fetch_one(pool)
    .await?;

    let (resolved_since, resolved_total, pending) = sqlx::query_as::<_, (i64, i64, i64)>(
        r#"
        SELECT count(*) FILTER (WHERE resolved_at >= $2)::bigint,
               count(*) FILTER (WHERE resolved_at IS NOT NULL)::bigint,
               count(*) FILTER (WHERE resolved_at IS NULL)::bigint
        FROM growth_evidence
        WHERE workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .bind(since)
    .fetch_one(pool)
    .await?;

    // A person's yes: approved from the queue by a human, not by a standing
    // grant or a ladder (`operator:*`).
    let (approved, median_hours, p90_hours) =
        sqlx::query_as::<_, (i64, Option<f64>, Option<f64>)>(
            r#"
            SELECT count(*)::bigint,
                   percentile_cont(0.5) WITHIN GROUP (ORDER BY hours),
                   percentile_cont(0.9) WITHIN GROUP (ORDER BY hours)
            FROM (
                SELECT EXTRACT(EPOCH FROM (approved_at - created_at)) / 3600.0 AS hours
                FROM autopilot_actions
                WHERE workspace_id = $1
                  AND approved_at >= $2
                  AND approved_by IS NOT NULL
                  AND approved_by NOT LIKE 'operator:%'
            ) AS latency
            "#,
        )
        .bind(workspace_id)
        .bind(since)
        .fetch_one(pool)
        .await?;

    let (awaiting, oldest_awaiting_hours) = sqlx::query_as::<_, (i64, Option<f64>)>(
        r#"
        SELECT count(*)::bigint,
               (EXTRACT(EPOCH FROM ($2 - min(created_at))) / 3600.0)::double precision
        FROM autopilot_actions
        WHERE workspace_id = $1 AND status = 'awaiting_approval'
        "#,
    )
    .bind(workspace_id)
    .bind(now)
    .fetch_one(pool)
    .await?;

    let cut_list = load_lanes_60d(pool, workspace_id, now).await?;

    let proof = load_fan_acquisition_proof(pool, workspace_id, since, now).await?;

    let reddit = load_reddit_standing(pool, workspace_id, now).await?;

    Ok(json!({
        "objective": objective,
        "pace": pace,
        "since": since,
        "planned": {
            "dispatches": dispatches,
            // Expected new fans the dispatches were decided on — not the
            // series' unit, so never subtracted from `actual`.
            "expected_new_fans": expected_new_fans,
        },
        "actual": {
            // Distance the objective's series has moved since its frozen
            // baseline, toward the target. Null with no objective or no
            // observation: an unread series is not zero movement.
            "travelled": travelled,
            "observed_value": objective.as_ref().and_then(|objective| objective.observed_value),
            "target_value": objective.as_ref().map(|objective| objective.target_value),
        },
        "learning": {
            "resolved_since": resolved_since,
            "resolved_total": resolved_total,
            "pending": pending,
            "target_resolved": LEARNING_TARGET_RESOLVED,
        },
        "approvals": {
            "approved_by_people": approved,
            "median_hours": median_hours,
            "p90_hours": p90_hours,
            "awaiting": awaiting,
            "oldest_awaiting_hours": oldest_awaiting_hours,
        },
        "lanes_60d": cut_list,
        "proof": proof,
        "reddit": reddit,
    }))
}

/// Hard causal proof that CrowdRelay found/acquired real people.
///
/// Two rails stay separate:
/// * FAN SCOUT is person-level: observed prospect -> provider-confirmed invite
///   touch -> that touch's Smart Link -> anonymous visitor -> first-party fan.
/// * action attribution is execution-level: conversion provenance carries the
///   exact autopilot action id.
///
/// Signal counts require an active endpoint created after the conversion. No
/// workspace delta, timing-only join or provider follower estimate is accepted.
async fn load_fan_acquisition_proof(
    pool: &PgPool,
    workspace_id: Uuid,
    since: OffsetDateTime,
    now: OffsetDateTime,
) -> Result<serde_json::Value, sqlx::Error> {
    let (
        prospects_discovered,
        invites_confirmed,
        prospect_fans,
        prospect_signal_fans,
    ) = sqlx::query_as::<_, (i64, i64, i64, i64)>(
        r#"
        WITH provider_prospects AS (
            SELECT DISTINCT observation.prospect_id
            FROM fan_prospect_observations AS observation
            JOIN community_comments AS comment
              ON comment.workspace_id = observation.workspace_id
             AND comment.id::text = observation.source_ref
             AND comment.provider_observed_at IS NOT NULL
             AND comment.provider_observed_at <= $3
            WHERE observation.workspace_id = $1
              AND observation.source = 'own_comments'
              AND observation.observed_at >= $2
              AND observation.observed_at <= $3
        ), confirmed_invites AS (
            SELECT DISTINCT
                   touch.id,
                   touch.prospect_id,
                   touch.smart_link_id,
                   touch.touched_at
            FROM fan_prospect_touches AS touch
            JOIN community_comments AS comment
              ON comment.workspace_id = touch.workspace_id
             AND comment.id::text = touch.source_ref
             AND comment.status = 'replied'
             AND comment.replied_at = touch.touched_at
             AND NULLIF(btrim(comment.reply_comment_id),'') IS NOT NULL
            JOIN smart_links AS link
              ON link.workspace_id = touch.workspace_id
             AND link.id = touch.smart_link_id
             AND link.active
             AND link.slug = 'reply-capture-' || replace(comment.id::text,'-','')
             AND position('/l/' || link.slug in COALESCE(comment.draft,'')) > 0
            JOIN provider_prospects AS discovered
              ON discovered.prospect_id = touch.prospect_id
            WHERE touch.workspace_id = $1
              AND touch.source = 'owned_reply'
              AND touch.kind = 'invite'
              AND touch.touched_at >= $2
              AND touch.touched_at <= $3
        ), exact_prospect_conversions AS (
            SELECT DISTINCT
                   touch.prospect_id,
                   canonical_fan_id($1, arrival.fan_id) AS fan_id,
                   MIN(arrival.occurred_at) AS converted_at
            FROM confirmed_invites AS touch
            JOIN click_events AS click
              ON click.workspace_id = $1
             AND click.smart_link_id = touch.smart_link_id
             AND click.anonymous_visitor_id IS NOT NULL
             AND click.occurred_at >= touch.touched_at
            JOIN fan_acquisition_events AS arrival
              ON arrival.workspace_id = touch.workspace_id
             AND arrival.anonymous_visitor_id = click.anonymous_visitor_id
             AND arrival.fan_id IS NOT NULL
             AND arrival.occurred_at >= click.occurred_at
             AND arrival.occurred_at <= click.occurred_at + interval '30 days'
            JOIN fan_prospects AS prospect
              ON prospect.workspace_id = touch.workspace_id
             AND prospect.id = touch.prospect_id
             AND prospect.status = 'converted'
             AND canonical_fan_id($1, prospect.linked_fan_id)
                   = canonical_fan_id($1, arrival.fan_id)
            WHERE arrival.occurred_at >= $2
              AND arrival.occurred_at <= $3
            GROUP BY touch.prospect_id, canonical_fan_id($1, arrival.fan_id)
        )
        SELECT
            (SELECT count(*)::bigint FROM provider_prospects),
            (SELECT count(*)::bigint FROM confirmed_invites),
            (SELECT count(*)::bigint
             FROM exact_prospect_conversions),
            (SELECT count(*)::bigint
             FROM exact_prospect_conversions AS conversion
             JOIN fans AS fan
               ON fan.workspace_id=$1
              AND fan.id=conversion.fan_id
              AND fan.status='active'
              AND fan.deleted_at IS NULL
              AND fan.merged_into_fan_id IS NULL
             WHERE EXISTS (
                 SELECT 1
                 FROM fan_push_endpoints AS endpoint
                 WHERE endpoint.workspace_id=$1
                   AND endpoint.audience_kind='fan'
                   AND canonical_fan_id($1,endpoint.fan_id)=conversion.fan_id
                   AND endpoint.active
                   AND endpoint.invalidated_at IS NULL
                   AND endpoint.created_at >= conversion.converted_at
                   AND endpoint.created_at <= $3
             ))
        "#,
    )
    .bind(workspace_id)
    .bind(since)
    .bind(now)
    .fetch_one(pool)
    .await?;

    let (action_fans, action_signal_fans) = sqlx::query_as::<_, (i64, i64)>(
        r#"
        WITH action_conversions AS (
            SELECT canonical_fan_id($1, conversion.fan_id) AS fan_id,
                   MIN(conversion.occurred_at) AS converted_at
            FROM fan_provenance_events AS conversion
            WHERE conversion.workspace_id=$1
              AND conversion.event_kind='conversion'
              AND conversion.attribution_method='last_tracked_click'
              AND conversion.action_id IS NOT NULL
              AND conversion.occurred_at >= $2
              AND conversion.occurred_at <= $3
            GROUP BY canonical_fan_id($1, conversion.fan_id)
        ), owned AS (
            SELECT conversion.fan_id, conversion.converted_at
            FROM action_conversions AS conversion
            JOIN fans AS fan
              ON fan.workspace_id=$1
             AND fan.id=conversion.fan_id
             AND fan.status='active'
             AND fan.deleted_at IS NULL
             AND fan.merged_into_fan_id IS NULL
            WHERE conversion.fan_id IS NOT NULL
        )
        SELECT
            (SELECT count(*)::bigint FROM owned),
            (SELECT count(*)::bigint
             FROM owned
             WHERE EXISTS (
                 SELECT 1
                 FROM fan_push_endpoints AS endpoint
                 WHERE endpoint.workspace_id=$1
                   AND endpoint.audience_kind='fan'
                   AND canonical_fan_id($1,endpoint.fan_id)=owned.fan_id
                   AND endpoint.active
                   AND endpoint.invalidated_at IS NULL
                   AND endpoint.created_at >= owned.converted_at
                   AND endpoint.created_at <= $3
             ))
        "#,
    )
    .bind(workspace_id)
    .bind(since)
    .bind(now)
    .fetch_one(pool)
    .await?;

    Ok(json!({
        "window": {"since": since, "observed_at": now},
        "fan_scout": {
            "prospects_discovered": prospects_discovered,
            // Written only after the sending surface says the reply is live.
            "provider_confirmed_invites": invites_confirmed,
            "exact_converted_fans": prospect_fans,
            "exact_signal_fans": prospect_signal_fans,
            "evidence_class": "person_touch_visitor_fan_exact",
        },
        "actions": {
            "exact_canonical_fans": action_fans,
            "exact_signal_fans": action_signal_fans,
            "evidence_class": "action_last_tracked_click_fan_exact",
        },
        "external_social": {
            "evidence_class": "provider_series_only_not_person_identity",
            "note": "Follower/subscriber series are reported elsewhere as provider-observed or experimental lift; they are never counted here as person-level acquired fans."
        }
    }))
}

/// Every lane (context × action kind) dispatched in the last 60 days — long
/// enough for a 30-day outcome to resolve on most of the window, short enough
/// that a lane cut last quarter does not linger — with what it produced.
async fn load_lanes_60d(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let lanes = sqlx::query_as::<_, (String, String, i64, i64, f64)>(
        r#"
        SELECT action.context::text,
               action.action_kind::text,
               count(*)::bigint,
               count(*) FILTER (WHERE evidence.resolved_at IS NOT NULL)::bigint,
               -- Only rows whose fans were traced to the action: summing
               -- workspace-window rows counts one arrival once per
               -- overlapping dispatch (`OutcomeBasis`).
               COALESCE(sum(evidence.observed_fans)
                        FILTER (WHERE evidence.resolved_at IS NOT NULL
                                  AND evidence.outcome_basis = 'attributed'), 0)::double precision
        FROM growth_evidence AS evidence
        JOIN autopilot_actions AS action
          ON action.workspace_id = evidence.workspace_id
         AND action.id = evidence.action_id
        WHERE evidence.workspace_id = $1
          AND evidence.timestamp >= $2 - interval '60 days'
          AND evidence.treatment = 'treatment'
        GROUP BY 1, 2
        ORDER BY 5 ASC, 4 DESC, 1, 2
        "#,
    )
    .bind(workspace_id)
    .bind(now)
    .fetch_all(pool)
    .await?;
    Ok(lanes
        .into_iter()
        .map(|(context, action_kind, dispatched, resolved, fans)| {
            json!({
                "context": context,
                "action_kind": action_kind,
                "dispatched": dispatched,
                "resolved": resolved,
                "fans": fans,
                "cut_candidate": resolved >= CUT_MIN_RESOLVED && fans <= 0.0,
            })
        })
        .collect())
}

/// The three outreach conditions a person should hear about without opening
/// the console, as Prometheus gauges. The heartbeat forwards them and the
/// Control Plane notifies on a rise:
///
/// - `crowdrelay_outreach_reddit_halted` — 1 when the breaker halted
///   unattended Reddit posting (a filter removal or repeated removals).
/// - `crowdrelay_outreach_replies_waiting_12h` — drafted answers to people
///   who commented, waiting on a person for more than 12 hours. A reply a day
///   late reads as a brand, not a band.
/// - `crowdrelay_outreach_lanes_cut_candidate` — lanes with enough resolved
///   outcomes and not one fan in 60 days.
pub(crate) async fn outreach_alert_prometheus(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<String, sqlx::Error> {
    let now = OffsetDateTime::now_utc();
    let reddit = load_reddit_standing(pool, workspace_id, now).await?;
    let halted = u8::from(reddit.get("state").and_then(Value::as_str) == Some("halted"));
    let waiting: i64 = sqlx::query_scalar(
        r#"
        SELECT count(*)::bigint FROM community_comments
        WHERE workspace_id = $1
          AND status = 'awaiting_approval'
          AND created_at < $2 - interval '12 hours'
        "#,
    )
    .bind(workspace_id)
    .bind(now)
    .fetch_one(pool)
    .await?;
    let cut = load_lanes_60d(pool, workspace_id, now)
        .await?
        .iter()
        .filter(|lane| lane.get("cut_candidate").and_then(Value::as_bool) == Some(true))
        .count();
    Ok(format!(
        concat!(
            "# HELP crowdrelay_outreach_reddit_halted 1 when unattended Reddit posting is halted by the account's standing.\n",
            "# TYPE crowdrelay_outreach_reddit_halted gauge\n",
            "crowdrelay_outreach_reddit_halted {}\n",
            "# HELP crowdrelay_outreach_replies_waiting_12h Drafted replies to commenters waiting on a person for over 12 hours.\n",
            "# TYPE crowdrelay_outreach_replies_waiting_12h gauge\n",
            "crowdrelay_outreach_replies_waiting_12h {}\n",
            "# HELP crowdrelay_outreach_lanes_cut_candidate Lanes with 5+ resolved outcomes and no fan in 60 days.\n",
            "# TYPE crowdrelay_outreach_lanes_cut_candidate gauge\n",
            "crowdrelay_outreach_lanes_cut_candidate {}\n",
        ),
        halted, waiting, cut
    ))
}

/// The standing the community executor applies, from the same 180-day post
/// history (`community_executor::standing::post_history`) and the same
/// domain rule, so the screen and the breaker cannot disagree.
async fn load_reddit_standing(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<serde_json::Value, sqlx::Error> {
    use crowdrelay_domain::reddit_standing::{
        PostRecord, RedditStanding, RemovalCause, SUBREDDIT_MEMORY, autonomy_proven,
        reddit_standing,
    };

    let rows = sqlx::query_as::<
        _,
        (
            String,
            OffsetDateTime,
            Option<String>,
            Option<OffsetDateTime>,
            Option<OffsetDateTime>,
            bool,
        ),
    >(
        r#"
        SELECT normalize_subreddit(post.subreddit), post.posted_at,
               post.removed_by_category, post.removal_seen_at, post.last_seen_live_at,
               (
                   COALESCE(latest.score > 1, false)
                   OR EXISTS (
                       SELECT 1
                       FROM community_comments AS human_comment
                       WHERE human_comment.workspace_id = post.workspace_id
                         AND human_comment.community_post_id = post.id
                   )
               )
        FROM community_posts AS post
        LEFT JOIN LATERAL (
            SELECT metric.score, metric.num_comments
            FROM community_post_metrics AS metric
            WHERE metric.workspace_id = post.workspace_id
              AND metric.community_post_id = post.id
            ORDER BY metric.measured_at DESC, metric.id DESC
            LIMIT 1
        ) AS latest ON true
        WHERE post.workspace_id = $1
          AND post.status = 'posted'
          AND post.posted_at IS NOT NULL
          AND posted_at > $2 - interval '180 days'
        "#,
    )
    .bind(workspace_id)
    .bind(now)
    .fetch_all(pool)
    .await?;
    let history: Vec<PostRecord> = rows
        .into_iter()
        .map(
            |(
                subreddit,
                posted_at,
                category,
                removal_seen_at,
                last_seen_live_at,
                community_responded,
            )| PostRecord {
                subreddit,
                posted_at,
                removal: category.as_deref().and_then(RemovalCause::from_category),
                removal_seen_at,
                last_seen_live_at,
                community_responded,
            },
        )
        .collect();
    let posted_24h = history
        .iter()
        .filter(|post| now - post.posted_at <= time::Duration::hours(24))
        .count();
    let mut removed_by: Vec<&str> = history
        .iter()
        .filter(|post| {
            post.removal.is_some_and(RemovalCause::is_verdict)
                && now - post.removal_seen_at.unwrap_or(post.posted_at) <= SUBREDDIT_MEMORY
        })
        .map(|post| post.subreddit.as_str())
        .collect();
    removed_by.sort_unstable();
    removed_by.dedup();
    let (state, daily_cap, halt_reason) = match reddit_standing(&history, now) {
        RedditStanding::Open { daily_cap } => ("open", Some(daily_cap), None),
        RedditStanding::Halted(reason) => ("halted", None, Some(reason.as_str())),
    };
    Ok(json!({
        "state": state,
        "daily_cap": daily_cap,
        "halt_reason": halt_reason,
        "posted_24h": posted_24h,
        "posts_180d": history.len(),
        "unattended_posting_earned": autonomy_proven(&history, now),
        "removed_by": removed_by,
    }))
}
