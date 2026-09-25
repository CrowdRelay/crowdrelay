// The "is it working" read: one row per action a person approved (or a
// standing grant let through) that reached a terminal state in the window,
// each with the measurement verdicts it has earned so far.
//
// The funnel answers "how much went out"; this answers "what came back".
// An action that has not been measured yet is `pending`, and one that
// never scheduled a measurement is `unmeasured` — the console prints the
// word, never a zero, because a measured absence and an unasked question
// are different statements.
//
// `label` is the best human name the payload carries — different kinds
// name their subject under different keys, so the read coalesces them
// rather than trusting one.

pub async fn outcomes(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id: Uuid = state.ops.workspace_id().into_uuid();
    match load_outcomes(&state.ops.pool, workspace_id).await {
        Ok(outcomes) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(outcomes),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "ops outcomes read failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}

async fn load_outcomes(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<serde_json::Value, sqlx::Error> {
    // Terminal actions a person put their name to, newest first. A
    // standing-grant send counts too — `approved_by` records the grant, so
    // the ledger already says which actions had a human's yes behind them
    // and which never did.
    let rows = sqlx::query(
        r#"
        SELECT a.id, a.action_kind, a.context, a.subject_kind, a.subject_id::text,
               COALESCE(
                   NULLIF(a.payload->>'target_name', ''),
                   NULLIF(a.payload->>'agent_name', ''),
                   NULLIF(a.payload->>'recipient_name', ''),
                   NULLIF(a.payload->>'title', ''),
                   NULLIF(a.payload->>'venue_name', ''),
                   a.subject_id::text
               ) AS label,
               a.status, a.approved_at, a.finished_at, a.last_error_kind,
               (
                   SELECT jsonb_agg(jsonb_build_object(
                       'metric', o.metric_key,
                       'verdict', o.effect_assessment,
                       'observed', o.observed_value,
                       'baseline', o.baseline_value,
                       'at', o.observed_at
                   ) ORDER BY o.observed_at DESC)
                   FROM autopilot_outcomes o
                   WHERE o.workspace_id = a.workspace_id
                     AND o.action_id = a.id
               ) AS outcome_rows,
               (
                   SELECT min(m.due_at)
                   FROM autopilot_measurements m
                   WHERE m.workspace_id = a.workspace_id
                     AND m.action_id = a.id
                     AND m.status = 'pending'
               ) AS next_measurement_due
        FROM autopilot_actions a
        WHERE a.workspace_id = $1
          AND a.approved_at IS NOT NULL
          AND a.status IN ('succeeded', 'failed')
          AND a.finished_at >= now() - interval '14 days'
        ORDER BY a.finished_at DESC, a.id DESC
        LIMIT 25
        "#,
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?;

    use sqlx::Row;
    let actions: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let outcome_rows: Option<serde_json::Value> = row.get("outcome_rows");
            let next_due: Option<time::OffsetDateTime> = row.get("next_measurement_due");
            let outcome_state = if outcome_rows.as_ref().is_some_and(|v| {
                v.as_array().is_some_and(|a| !a.is_empty())
            }) {
                "measured"
            } else if next_due.is_some() {
                "pending"
            } else {
                "unmeasured"
            };
            json!({
                "id": row.get::<Uuid, _>("id"),
                "kind": row.get::<String, _>("action_kind"),
                "context": row.get::<String, _>("context"),
                "label": row.get::<Option<String>, _>("label"),
                "status": row.get::<String, _>("status"),
                "error_kind": row.get::<Option<String>, _>("last_error_kind"),
                "approved_at": row.get::<Option<time::OffsetDateTime>, _>("approved_at"),
                "finished_at": row.get::<Option<time::OffsetDateTime>, _>("finished_at"),
                "outcomes": outcome_rows.unwrap_or(serde_json::Value::Array(vec![])),
                "outcome_state": outcome_state,
                "next_measurement_due": next_due,
            })
        })
        .collect();

    Ok(json!({
        "window_days": 14,
        "actions": actions,
    }))
}
