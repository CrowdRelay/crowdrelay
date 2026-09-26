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
// With no live objective the window is the trailing 21 days, so the learning
// and approval numbers still answer before anybody declares a target.

/// Resolved evidence rows the brain needs before uncertainty can enter
/// selection (see docs/BRAIN_LEARNING_LOOP.md, known weakness 1).
const LEARNING_TARGET_RESOLVED: i64 = 200;
/// The window used when no objective is live — the run length the scoreboard
/// exists for.
const DEFAULT_GOAL_WINDOW_DAYS: i64 = 21;

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
    }))
}
