// "Who can help with this show" (§4h-11, 3.10) — the staging queue read
// against a date rather than as an inventory.
//
// The same night the timeline describes, from the other side: not what the
// band owes the show, but who in that city could make more people come to it.
// Everything here is a candidate. Promotion, the contact governor and the
// admission wall all still stand between this list and anybody being written
// to — and the response carries no addresses, because a shortlist names who,
// it does not hand out a way to reach them.

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
        // `None` is only "no such event". A show with no city still answers:
        // `degraded: ["city"]` and every section empty — a band needs to know
        // the city is missing, not be handed a 404 that reads as "no show".
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
