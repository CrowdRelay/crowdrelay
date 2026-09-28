/// The operator's "promote this drop now" on one content source.
///
/// Stamps the source's surge request and wakes the worker through the same
/// NOTIFY a cycle request sends — so the 202 means "the surge was requested
/// and the cycle will see it", never that a post went out. The lanes each
/// carry their own approval and dedupe semantics; lanes that already
/// delivered for this source keep their keys and do not re-send, so a second
/// promote of the same drop is safe.
///
/// Only `video` and `release` sources promote — those are the kinds the
/// drop-surge window evaluates.
pub async fn promote_content_source(
    State(state): State<AppState>,
    Path(source_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    // A disabled autopilot must not be startable from the outside — same
    // master-switch check the cycle-run button makes.
    if !state.autopilot_runtime_enabled {
        return Problem::conflict(request_id(&headers)).into_response();
    }
    let Ok(source_id) = Uuid::parse_str(&source_id) else {
        return Problem::not_found(request_id(&headers))
            .private()
            .into_response();
    };
    match crowdrelay_infra::autopilot::request_drop_surge(
        state.autopilot.pool(),
        state.ops.workspace_id(),
        source_id,
    )
    .await
    {
        Ok(request) => private_json(
            StatusCode::ACCEPTED,
            serde_json::json!({
                "status": "requested",
                "surge": request,
                "detail": "the worker runs the surge on its next cycle; watch actions and the operator feed for what each lane did",
            }),
        ),
        Err(error) => repository_problem(error, request_id(&headers)),
    }
}
