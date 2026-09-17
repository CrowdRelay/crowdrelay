// "Who can help with this show" (§4h-11, 3.10) — the staging queue read
// against a date rather than as an inventory.
//
// The same night the timeline describes, from the other side: not what the
// band owes the show, but who in that city could make more people come to it.
// Everything here is a candidate. Promotion, the contact governor and the
// admission wall all still stand between this list and anybody being written
// to, which is why the read carries a state per row instead of an action.

/// `GET /v1/control-plane/events/{event_slug}/who-can-help`
pub async fn control_plane_event_helpers(
    State(state): State<crate::AppState>,
    Path(event_slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    match crate::ops::hold(
        &state.read_budget,
        crowdrelay_infra::show_helpers::who_can_help(&state.database, workspace_id, &event_slug),
    )
    .await
    {
        // A show with no city resolves to `None` alongside a show that does
        // not exist: without a city there is nobody local, and answering with
        // the whole contact list is the inventory this read replaces.
        Ok(Some(helpers)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(helpers),
        )
            .into_response(),
        Ok(None) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "control-plane show helpers query failed");
            Problem::service_unavailable(request_id_value)
                .private()
                .into_response()
        }
    }
}
