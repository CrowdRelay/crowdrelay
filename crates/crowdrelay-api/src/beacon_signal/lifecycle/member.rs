use super::*;

use crowdrelay_application::{LeaveCommand, RecordEngagementCommand, SubmitCoverageCommand};

pub async fn press_room(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Query(query): Query<PressRoomQuery>,
) -> Response {
    let request_id_value = request_id(&headers);
    let principal = match authorize_beacon(&state, &headers).await {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let event = if let Some(event_id) = query.event_id {
        let event = sqlx::query_as::<_, PressRoomEventView>(
            r#"
            SELECT event.id,event.slug,event.title,event.venue,city.name AS city,
                   event.starts_at,event.doors_at,event.ticket_url,event.description,
                   event.image_url,event.listen_url,event.trailer_url
            FROM events event
            LEFT JOIN cities city ON city.id=event.city_id
            WHERE event.workspace_id=$1 AND event.id=$2
            "#,
        )
        .bind(workspace_id)
        .bind(event_id)
        .fetch_optional(state.ticketing.pool())
        .await;
        match event {
            Ok(Some(event)) => Some(event),
            Ok(None) => return BeaconSignalError::NotFound.response(request_id_value),
            Err(error) => {
                tracing::warn!(%error, %event_id, "Beacon press-room event lookup failed");
                return BeaconSignalError::Unavailable.response(request_id_value);
            }
        }
    } else {
        None
    };
    let assets = sqlx::query_as::<_, PressAssetView>(
        r#"
        SELECT id,event_id,asset_key,asset_kind,label_pl,label_en,url,sort_order
        FROM viryaos_beacon_press_assets
        WHERE workspace_id=$1 AND active
          AND (event_id IS NULL OR event_id=$2)
        ORDER BY (event_id IS NOT NULL) DESC, sort_order, asset_key, id
        "#,
    )
    .bind(workspace_id)
    .bind(query.event_id)
    .fetch_all(state.ticketing.pool())
    .await;
    match assets {
        Ok(assets) => {
            tracing::debug!(beacon_id=%principal.beacon_id, asset_count=assets.len(), "Beacon press room read");
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(PressRoomResponse {
                    version: 2,
                    event_id: query.event_id,
                    event,
                    assets,
                }),
            )
                .into_response()
        }
        Err(error) => {
            tracing::warn!(%error, beacon_id=%principal.beacon_id, "Beacon press room read failed");
            BeaconSignalError::Unavailable.response(request_id_value)
        }
    }
}

pub async fn my_press_requests(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let principal = match authorize_beacon(&state, &headers).await {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    let requests = sqlx::query_as::<_, MyPressRequestView>(
        r#"
        SELECT request.id,request.event_id,event.title AS event_title,request.request_kind,
               request.details,request.status,request.resolution_note,request.created_at,request.resolved_at
        FROM viryaos_beacon_press_requests request
        LEFT JOIN events event
          ON event.workspace_id=request.workspace_id AND event.id=request.event_id
        WHERE request.workspace_id=$1 AND request.beacon_id=$2
        ORDER BY CASE request.status WHEN 'open' THEN 0 ELSE 1 END,
                 request.created_at DESC,request.id DESC
        LIMIT 100
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(principal.beacon_id)
    .fetch_all(state.ticketing.pool())
    .await;
    match requests {
        Ok(requests) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(MyPressRequestsResponse { requests }),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, beacon_id=%principal.beacon_id, "Beacon own press-request list failed");
            BeaconSignalError::Unavailable.response(request_id_value)
        }
    }
}

pub async fn record_event_engagement(
    State(state): State<crate::AppState>,
    Path(event_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<EngagementRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let principal = match authorize_beacon(&state, &headers).await {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    let help_details = match clean_optional_text(payload.help_details, 1500) {
        Some(value) => value,
        None => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    if payload.action == EngagementAction::Helping && payload.help_kind.is_none() {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    if payload.action != EngagementAction::Helping && payload.help_kind.is_some() {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    let command = RecordEngagementCommand {
        workspace_id: state.ticketing.workspace_id().into_uuid(),
        beacon_id: principal.beacon_id,
        event_id,
        radius_km: principal.radius_km,
        action: payload.action.as_str().to_owned(),
        help_kind: payload.help_kind.map(|value| value.as_str().to_owned()),
        help_details,
        request_id_header: request_id(&headers),
    };
    match state.beacon_release.record_event_engagement(&command).await {
        Ok(result) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(EngagementResponse {
                event_id,
                status: result.status,
                help_kind: result.help_kind,
            }),
        )
            .into_response(),
        Err(error) => map_repo_error(error).response(request_id_value),
    }
}

pub async fn submit_coverage(
    State(state): State<crate::AppState>,
    Path(event_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<CoverageRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let principal = match authorize_beacon(&state, &headers).await {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    let url = payload.url.trim().to_owned();
    let title = match clean_optional_text(payload.title, 240) {
        Some(value) => value,
        None => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    if !valid_https_url(&url) {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    let command = SubmitCoverageCommand {
        workspace_id: state.ticketing.workspace_id().into_uuid(),
        beacon_id: principal.beacon_id,
        event_id,
        coverage_kind: payload.coverage_kind.as_str().to_owned(),
        url,
        title,
        request_id_header: request_id(&headers),
    };
    match state.beacon_release.submit_coverage(&command).await {
        Ok(result) => (
            StatusCode::CREATED,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(CoverageResponse {
                coverage_id: result.coverage_id,
                event_id,
                status: "completed",
            }),
        )
            .into_response(),
        Err(error) => map_repo_error(error).response(request_id_value),
    }
}

pub async fn leave(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<LeaveRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let principal = match authorize_beacon(&state, &headers).await {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    let command = LeaveCommand {
        workspace_id: state.ticketing.workspace_id().into_uuid(),
        beacon_id: principal.beacon_id,
        do_not_contact: payload.do_not_contact,
        request_id_header: request_id(&headers),
    };
    match state.beacon_release.leave(&command).await {
        Ok(()) => (StatusCode::NO_CONTENT, [(CACHE_CONTROL, PRIVATE_NO_STORE)]).into_response(),
        Err(error) => map_repo_error(error).response(request_id_value),
    }
}
