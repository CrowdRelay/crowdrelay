// Operator-initiated autopilot cycles.
//
// Two endpoints, one safe and one not:
//
// - `preview_autopilot_cycle` is read-only. It reports what the brain believes
//   and which strategy that implies, dispatching nothing.
// - `run_autopilot_cycle` asks the worker to run a real cycle, which may
//   dispatch real outreach.
//
// The run endpoint does no evaluation here. It sends a NOTIFY and returns 202;
// the worker's loop wakes and runs the identical code path a scheduled tick
// runs. The 24-hour action quota is enforced in the transaction that writes an
// action, so this cannot outrun the guardrails — there is deliberately no
// second execution path that would have to be kept in step with the first.

/// Reports what a cycle would decide, without running one.
///
/// Safe to call at any cadence: no writes, no dispatch.
pub async fn preview_autopilot_cycle(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    match crowdrelay_infra::autopilot::preview_autopilot_cycle(
        &state.autopilot,
        state.ops.workspace_id(),
        OffsetDateTime::now_utc(),
    )
    .await
    {
        Ok(preview) => private_json(StatusCode::OK, preview),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}

/// Asks the worker to run a full autopilot cycle now.
///
/// Returns 202: the cycle runs in the worker, not in this request. The
/// response says the request was accepted, never that anything was dispatched
/// — what a cycle does depends on policy, quota and what the brain decides, and
/// claiming otherwise from here would be a guess.
pub async fn run_autopilot_cycle(State(state): State<AppState>, headers: HeaderMap) -> Response {
    // A disabled autopilot must not be startable from the outside. Without
    // this the button would be a way around the master switch.
    if !state.autopilot_runtime_enabled {
        return Problem::conflict(request_id(&headers)).into_response();
    }
    match crowdrelay_infra::autopilot::request_autopilot_cycle(
        state.autopilot.pool(),
        state.ops.workspace_id(),
    )
    .await
    {
        Ok(()) => private_json(
            StatusCode::ACCEPTED,
            serde_json::json!({
                "status": "requested",
                "detail": "the worker runs the cycle; watch actions and the operator feed for what it decided",
            }),
        ),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}
