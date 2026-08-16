use super::*;

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
                Json(PressRoomResponse { version: 2, event_id: query.event_id, event, assets }),
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
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Beacon engagement transaction failed to start");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let allowed = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM events event
            JOIN cities event_city ON event_city.id=event.city_id
            JOIN viryaos_beacons beacon ON beacon.workspace_id=event.workspace_id AND beacon.id=$2
            JOIN cities home_city ON home_city.id=beacon.city_id
            WHERE event.workspace_id=$1 AND event.id=$3
              AND event.status='published'
              AND event.starts_at > now() - interval '2 days'
              AND home_city.latitude IS NOT NULL AND home_city.longitude IS NOT NULL
              AND event_city.latitude IS NOT NULL AND event_city.longitude IS NOT NULL
              AND (6371 * 2 * ASIN(LEAST(1.0, SQRT(
                    POWER(SIN(RADIANS(home_city.latitude - event_city.latitude) / 2), 2)
                    + COS(RADIANS(event_city.latitude)) * COS(RADIANS(home_city.latitude))
                    * POWER(SIN(RADIANS(home_city.longitude - event_city.longitude) / 2), 2)
                  )))) <= $4
            UNION ALL
            SELECT 1 FROM viryaos_beacon_signal_event_engagements engagement
            WHERE engagement.workspace_id=$1 AND engagement.beacon_id=$2 AND engagement.event_id=$3
        )
        "#,
    )
    .bind(workspace_id)
    .bind(principal.beacon_id)
    .bind(event_id)
    .bind(principal.radius_km)
    .fetch_one(&mut *tx)
    .await;
    match allowed {
        Ok(true) => {}
        Ok(false) => return BeaconSignalError::NotFound.response(request_id_value),
        Err(error) => {
            tracing::warn!(%error, %event_id, "Beacon engagement eligibility lookup failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    }
    let current = sqlx::query_scalar::<_, String>(
        "SELECT status FROM viryaos_beacon_signal_event_engagements WHERE workspace_id=$1 AND beacon_id=$2 AND event_id=$3 FOR UPDATE",
    )
    .bind(workspace_id)
    .bind(principal.beacon_id)
    .bind(event_id)
    .fetch_optional(&mut *tx)
    .await;
    let current = match current {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, %event_id, "Beacon engagement current-state lookup failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let next_status = match next_engagement_status(current.as_deref(), payload.action) {
        Ok(value) => value,
        Err(()) => return BeaconSignalError::Conflict.response(request_id_value),
    };
    let help_kind = payload.help_kind.map(|value| value.as_str());
    let upsert = sqlx::query(
        r#"
        INSERT INTO viryaos_beacon_signal_event_engagements (
            workspace_id,beacon_id,event_id,status,help_kind,help_details,
            first_opened_at,last_opened_at,interested_at,helping_at,completed_at,declined_at
        ) VALUES (
            $1,$2,$3,$4,$5,$6,
            CASE WHEN $7='opened' THEN now() END,
            CASE WHEN $7='opened' THEN now() END,
            CASE WHEN $7='interested' THEN now() END,
            CASE WHEN $7='helping' THEN now() END,
            CASE WHEN $7='completed' THEN now() END,
            CASE WHEN $7='declined' THEN now() END
        )
        ON CONFLICT (workspace_id,beacon_id,event_id) DO UPDATE SET
            status=$4,
            help_kind=CASE WHEN $7='helping' THEN $5 ELSE viryaos_beacon_signal_event_engagements.help_kind END,
            help_details=CASE WHEN $7='helping' THEN $6 ELSE viryaos_beacon_signal_event_engagements.help_details END,
            first_opened_at=CASE WHEN $7='opened' THEN COALESCE(viryaos_beacon_signal_event_engagements.first_opened_at,now()) ELSE viryaos_beacon_signal_event_engagements.first_opened_at END,
            last_opened_at=CASE WHEN $7='opened' THEN now() ELSE viryaos_beacon_signal_event_engagements.last_opened_at END,
            interested_at=CASE WHEN $7='interested' THEN COALESCE(viryaos_beacon_signal_event_engagements.interested_at,now()) ELSE viryaos_beacon_signal_event_engagements.interested_at END,
            helping_at=CASE WHEN $7='helping' THEN COALESCE(viryaos_beacon_signal_event_engagements.helping_at,now()) ELSE viryaos_beacon_signal_event_engagements.helping_at END,
            completed_at=CASE WHEN $7='completed' THEN COALESCE(viryaos_beacon_signal_event_engagements.completed_at,now()) ELSE viryaos_beacon_signal_event_engagements.completed_at END,
            declined_at=CASE WHEN $7='declined' THEN COALESCE(viryaos_beacon_signal_event_engagements.declined_at,now()) ELSE viryaos_beacon_signal_event_engagements.declined_at END,
            updated_at=now()
        "#,
    )
    .bind(workspace_id)
    .bind(principal.beacon_id)
    .bind(event_id)
    .bind(next_status)
    .bind(help_kind)
    .bind(&help_details)
    .bind(payload.action.as_str())
    .execute(&mut *tx)
    .await;
    if let Err(error) = upsert {
        tracing::warn!(%error, %event_id, "Beacon engagement persistence failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    let (campaign_status, campaign_disposition) = match next_status {
        "eligible" | "notified" | "opened" => ("contacted", "received"),
        "interested" => ("interested", "interested"),
        "helping" => ("partner", "partner"),
        "completed" => ("closed", "partner"),
        "declined" => ("declined", "declined"),
        _ => return BeaconSignalError::Conflict.response(request_id_value),
    };
    if let Err(error) = sqlx::query(
        r#"
        INSERT INTO viryaos_beacon_campaigns (
            workspace_id,beacon_id,event_id,status,last_phase,last_reply_disposition,last_outreach_at
        ) VALUES ($1,$2,$3,$4,'local_push',$5,now())
        ON CONFLICT (workspace_id,beacon_id,event_id) DO UPDATE SET
            status=CASE
                WHEN $4='closed' THEN 'closed'
                WHEN $4='declined' THEN 'declined'
                WHEN $4='partner' THEN 'partner'
                WHEN viryaos_beacon_campaigns.status='partner' THEN 'partner'
                WHEN $4='interested' THEN 'interested'
                WHEN viryaos_beacon_campaigns.status='interested' THEN 'interested'
                WHEN viryaos_beacon_campaigns.status='declined' THEN 'declined'
                ELSE 'contacted'
            END,
            last_phase='local_push',
            last_reply_disposition=CASE
                WHEN $4 IN ('closed','declined','partner','interested') THEN $5
                WHEN viryaos_beacon_campaigns.status='partner' THEN 'partner'
                WHEN viryaos_beacon_campaigns.status='interested' THEN 'interested'
                WHEN viryaos_beacon_campaigns.status='declined' THEN 'declined'
                ELSE 'received'
            END,
            last_outreach_at=COALESCE(viryaos_beacon_campaigns.last_outreach_at,now()),
            updated_at=now()
        WHERE viryaos_beacon_campaigns.status NOT IN ('suppressed','closed')
        "#,
    )
    .bind(workspace_id)
    .bind(principal.beacon_id)
    .bind(event_id)
    .bind(campaign_status)
    .bind(campaign_disposition)
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, %event_id, "Beacon engagement campaign sync failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    let event_payload = serde_json::json!({
        "beacon_id": principal.beacon_id,
        "event_id": event_id,
        "status": next_status,
        "action": payload.action.as_str(),
        "help_kind": help_kind,
        "help_details": help_details,
    });
    if let Err(error) = sqlx::query(
        "INSERT INTO outbox_events (workspace_id,event_type,event_version,payload,request_id) VALUES ($1,'viryaos.beacon.signal_engagement_recorded',1,$2,$3)",
    )
    .bind(workspace_id)
    .bind(event_payload)
    .bind(request_id(&headers))
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, %event_id, "Beacon engagement outbox write failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    if let Err(error) = tx.commit().await {
        tracing::warn!(%error, %event_id, "Beacon engagement transaction failed to commit");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(EngagementResponse {
            event_id,
            status: next_status.to_owned(),
            help_kind: help_kind.map(str::to_owned),
        }),
    )
        .into_response()
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
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Beacon coverage transaction failed to start");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let engagement_status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM viryaos_beacon_signal_event_engagements WHERE workspace_id=$1 AND beacon_id=$2 AND event_id=$3 FOR UPDATE",
    )
    .bind(workspace_id)
    .bind(principal.beacon_id)
    .bind(event_id)
    .fetch_optional(&mut *tx)
    .await;
    match engagement_status {
        Ok(Some(status)) if status != "declined" => {}
        Ok(Some(_)) => return BeaconSignalError::Conflict.response(request_id_value),
        Ok(None) => return BeaconSignalError::NotFound.response(request_id_value),
        Err(error) => {
            tracing::warn!(%error, %event_id, "Beacon coverage engagement lookup failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    }
    let coverage_id = Uuid::now_v7();
    let coverage_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO viryaos_beacon_signal_coverage
            (id,workspace_id,beacon_id,event_id,coverage_kind,url,title)
        VALUES ($1,$2,$3,$4,$5,$6,$7)
        ON CONFLICT (workspace_id,beacon_id,event_id,url) DO UPDATE SET
            title=COALESCE(EXCLUDED.title,viryaos_beacon_signal_coverage.title)
        RETURNING id
        "#,
    )
    .bind(coverage_id)
    .bind(workspace_id)
    .bind(principal.beacon_id)
    .bind(event_id)
    .bind(payload.coverage_kind.as_str())
    .bind(&url)
    .bind(&title)
    .fetch_one(&mut *tx)
    .await;
    let coverage_id = match coverage_id {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, %event_id, "Beacon coverage persistence failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    if let Err(error) = sqlx::query(
        r#"
        UPDATE viryaos_beacon_signal_event_engagements
        SET status='completed',completed_at=COALESCE(completed_at,now()),updated_at=now()
        WHERE workspace_id=$1 AND beacon_id=$2 AND event_id=$3 AND status <> 'declined'
        "#,
    )
    .bind(workspace_id)
    .bind(principal.beacon_id)
    .bind(event_id)
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, %event_id, "Beacon coverage engagement completion failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    if let Err(error) = sqlx::query(
        r#"
        UPDATE viryaos_beacon_campaigns
        SET status='closed',last_reply_disposition='partner',updated_at=now()
        WHERE workspace_id=$1 AND beacon_id=$2 AND event_id=$3
          AND status <> 'suppressed'
        "#,
    )
    .bind(workspace_id)
    .bind(principal.beacon_id)
    .bind(event_id)
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, %event_id, "Beacon coverage campaign completion failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    let event_payload = serde_json::json!({
        "coverage_id": coverage_id,
        "beacon_id": principal.beacon_id,
        "event_id": event_id,
        "coverage_kind": payload.coverage_kind.as_str(),
        "url": url,
        "title": title,
    });
    if let Err(error) = sqlx::query(
        "INSERT INTO outbox_events (workspace_id,event_type,event_version,payload,request_id) VALUES ($1,'viryaos.beacon.coverage_submitted',1,$2,$3)",
    )
    .bind(workspace_id)
    .bind(event_payload)
    .bind(request_id(&headers))
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, %event_id, "Beacon coverage outbox write failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    if let Err(error) = tx.commit().await {
        tracing::warn!(%error, %event_id, "Beacon coverage transaction failed to commit");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    (
        StatusCode::CREATED,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(CoverageResponse { coverage_id, event_id, status: "completed" }),
    )
        .into_response()
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
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Beacon leave transaction failed to start");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    for result in [
        sqlx::query(
            "UPDATE viryaos_beacon_signal_profiles SET status='revoked',invite_token_hash=NULL,invite_expires_at=NULL,revoked_at=now(),paused_at=NULL,updated_at=now() WHERE workspace_id=$1 AND beacon_id=$2",
        )
        .bind(workspace_id)
        .bind(principal.beacon_id)
        .execute(&mut *tx)
        .await,
        sqlx::query(
            "UPDATE viryaos_beacon_signal_sessions SET revoked_at=COALESCE(revoked_at,now()) WHERE workspace_id=$1 AND beacon_id=$2 AND revoked_at IS NULL",
        )
        .bind(workspace_id)
        .bind(principal.beacon_id)
        .execute(&mut *tx)
        .await,
        sqlx::query(
            r#"
            UPDATE fan_push_endpoints endpoint
            SET active=false,invalidated_at=COALESCE(invalidated_at,now()),updated_at=now()
            WHERE endpoint.workspace_id=$1 AND endpoint.audience_kind='beacon' AND endpoint.active
              AND endpoint.principal_hash IN (
                  SELECT session.token_hash FROM viryaos_beacon_signal_sessions session
                  WHERE session.workspace_id=$1 AND session.beacon_id=$2
              )
            "#,
        )
        .bind(workspace_id)
        .bind(principal.beacon_id)
        .execute(&mut *tx)
        .await,
    ] {
        if let Err(error) = result {
            tracing::warn!(%error, beacon_id=%principal.beacon_id, "Beacon leave state mutation failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    }
    if payload.do_not_contact {
        if let Err(error) = sqlx::query(
            "UPDATE viryaos_beacons SET accepts_outreach=false,do_not_contact=true,version=version+1,updated_at=now() WHERE workspace_id=$1 AND id=$2",
        )
        .bind(workspace_id)
        .bind(principal.beacon_id)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, beacon_id=%principal.beacon_id, "Beacon global do-not-contact update failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    }
    let event_payload = serde_json::json!({
        "beacon_id": principal.beacon_id,
        "do_not_contact": payload.do_not_contact,
    });
    if let Err(error) = sqlx::query(
        "INSERT INTO outbox_events (workspace_id,event_type,event_version,payload,request_id) VALUES ($1,'viryaos.beacon.signal_left',1,$2,$3)",
    )
    .bind(workspace_id)
    .bind(event_payload)
    .bind(request_id(&headers))
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, beacon_id=%principal.beacon_id, "Beacon leave outbox write failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    if let Err(error) = tx.commit().await {
        tracing::warn!(%error, beacon_id=%principal.beacon_id, "Beacon leave transaction failed to commit");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    (StatusCode::NO_CONTENT, [(CACHE_CONTROL, PRIVATE_NO_STORE)]).into_response()
}

