use super::*;

pub async fn create_invite_batch(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<BatchInviteRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    let Some(locale) = clean_locale(&payload.locale) else {
        return BeaconSignalError::BadRequest.response(request_id_value);
    };
    if payload.beacon_ids.is_empty()
        || payload.beacon_ids.len() > MAX_BATCH_INVITES
        || !(1..=MAX_INVITE_TTL_DAYS).contains(&payload.ttl_days)
        || !valid_radius(payload.radius_km)
    {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    let mut beacon_ids = payload.beacon_ids;
    beacon_ids.sort_unstable();
    beacon_ids.dedup();
    if beacon_ids.len() > MAX_BATCH_INVITES {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }

    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Beacon batch invite transaction failed to start");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let response = match mint_invite_batch_tx(
        &mut tx,
        workspace_id,
        &beacon_ids,
        payload.ttl_days,
        payload.radius_km,
        &locale,
    )
    .await
    {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    if let Err(error) = tx.commit().await {
        tracing::warn!(%error, "Beacon batch invite transaction failed to commit");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    (
        StatusCode::CREATED,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(response),
    )
        .into_response()
}

pub async fn admin_candidates(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let rows = sqlx::query_as::<_, AdminCandidateView>(
        r#"
        SELECT beacon.id AS beacon_id, beacon.display_name, beacon.beacon_kind,
               beacon.contact_email, city.name AS city, beacon.relevance_basis_points,
               beacon.relationship_score, profile.status AS signal_status,
               COALESCE(profile.invite_count,0)::integer AS invite_count,
               profile.last_invited_at
        FROM viryaos_beacons beacon
        LEFT JOIN cities city ON city.id=beacon.city_id
        LEFT JOIN viryaos_beacon_signal_profiles profile
          ON profile.workspace_id=beacon.workspace_id AND profile.beacon_id=beacon.id
        WHERE beacon.workspace_id=$1
          AND beacon.active AND beacon.verified AND beacon.accepts_outreach
          AND NOT beacon.do_not_contact AND beacon.contact_email IS NOT NULL
          AND COALESCE(profile.status,'') <> 'active'
        ORDER BY beacon.relevance_basis_points DESC, beacon.relationship_score DESC,
                 beacon.display_name, beacon.id
        LIMIT 500
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_all(state.ticketing.pool())
    .await;
    match rows {
        Ok(candidates) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(AdminCandidatesResponse { candidates }),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "Beacon Signal candidate list failed");
            BeaconSignalError::Unavailable.response(request_id_value)
        }
    }
}

pub async fn admin_dashboard(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let profiles = sqlx::query_as::<_, AdminProfileView>(
        r#"
        WITH session_counts AS (
            SELECT workspace_id,beacon_id,count(*)::bigint AS active_sessions
            FROM viryaos_beacon_signal_sessions
            WHERE revoked_at IS NULL AND expires_at > now()
            GROUP BY workspace_id,beacon_id
        ), endpoint_counts AS (
            SELECT session.workspace_id,session.beacon_id,count(DISTINCT endpoint.id)::bigint AS active_push_endpoints
            FROM viryaos_beacon_signal_sessions session
            JOIN fan_push_endpoints endpoint
              ON endpoint.workspace_id=session.workspace_id
             AND endpoint.audience_kind='beacon'
             AND endpoint.principal_hash=session.token_hash
             AND endpoint.active AND endpoint.invalidated_at IS NULL
            WHERE session.revoked_at IS NULL AND session.expires_at > now()
            GROUP BY session.workspace_id,session.beacon_id
        ), request_counts AS (
            SELECT workspace_id,beacon_id,count(*)::bigint AS open_press_requests
            FROM viryaos_beacon_press_requests WHERE status='open'
            GROUP BY workspace_id,beacon_id
        ), engagement_counts AS (
            SELECT workspace_id,beacon_id,count(*)::bigint AS active_engagements
            FROM viryaos_beacon_signal_event_engagements
            WHERE status NOT IN ('completed','declined')
            GROUP BY workspace_id,beacon_id
        ), coverage_counts AS (
            SELECT workspace_id,beacon_id,count(*)::bigint AS coverage_count
            FROM viryaos_beacon_signal_coverage GROUP BY workspace_id,beacon_id
        )
        SELECT profile.beacon_id,beacon.display_name,beacon.beacon_kind,beacon.contact_email,
               city.name AS city,profile.status,profile.radius_km,profile.locale,
               profile.nearby_gigs_enabled,profile.invite_count,profile.last_invited_at,
               profile.joined_at,profile.last_seen_at,
               COALESCE(session_counts.active_sessions,0)::bigint AS active_sessions,
               COALESCE(endpoint_counts.active_push_endpoints,0)::bigint AS active_push_endpoints,
               COALESCE(request_counts.open_press_requests,0)::bigint AS open_press_requests,
               COALESCE(engagement_counts.active_engagements,0)::bigint AS active_engagements,
               COALESCE(coverage_counts.coverage_count,0)::bigint AS coverage_count
        FROM viryaos_beacon_signal_profiles profile
        JOIN viryaos_beacons beacon
          ON beacon.workspace_id=profile.workspace_id AND beacon.id=profile.beacon_id
        LEFT JOIN cities city ON city.id=beacon.city_id
        LEFT JOIN session_counts USING (workspace_id,beacon_id)
        LEFT JOIN endpoint_counts USING (workspace_id,beacon_id)
        LEFT JOIN request_counts USING (workspace_id,beacon_id)
        LEFT JOIN engagement_counts USING (workspace_id,beacon_id)
        LEFT JOIN coverage_counts USING (workspace_id,beacon_id)
        WHERE profile.workspace_id=$1
        ORDER BY CASE profile.status WHEN 'active' THEN 0 WHEN 'invited' THEN 1 WHEN 'paused' THEN 2 ELSE 3 END,
                 beacon.relevance_basis_points DESC,beacon.relationship_score DESC,beacon.display_name,beacon.id
        LIMIT 500
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_all(state.ticketing.pool())
    .await;
    match profiles {
        Ok(profiles) => {
            let active = profiles
                .iter()
                .filter(|profile| profile.status == "active")
                .count();
            let invited = profiles
                .iter()
                .filter(|profile| profile.status == "invited")
                .count();
            let paused = profiles
                .iter()
                .filter(|profile| profile.status == "paused")
                .count();
            let revoked = profiles
                .iter()
                .filter(|profile| profile.status == "revoked")
                .count();
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(AdminDashboardResponse {
                    total: profiles.len(),
                    active,
                    invited,
                    paused,
                    revoked,
                    profiles,
                }),
            )
                .into_response()
        }
        Err(error) => {
            tracing::warn!(%error, "Beacon Signal admin dashboard failed");
            BeaconSignalError::Unavailable.response(request_id_value)
        }
    }
}

pub async fn admin_set_state(
    State(state): State<crate::AppState>,
    Path(beacon_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<AdminProfileStateRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, %beacon_id, "Beacon admin-state transaction failed to start");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let row = sqlx::query_as::<_, (String, Option<OffsetDateTime>, bool, bool, bool, bool)>(
        r#"
        SELECT profile.status,profile.joined_at,beacon.active,beacon.verified,
               beacon.accepts_outreach,beacon.do_not_contact
        FROM viryaos_beacon_signal_profiles profile
        JOIN viryaos_beacons beacon
          ON beacon.workspace_id=profile.workspace_id AND beacon.id=profile.beacon_id
        WHERE profile.workspace_id=$1 AND profile.beacon_id=$2
        FOR UPDATE OF profile,beacon
        "#,
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .fetch_optional(&mut *tx)
    .await;
    let (_current, joined_at, active, verified, accepts_outreach, do_not_contact) = match row {
        Ok(Some(value)) => value,
        Ok(None) => return BeaconSignalError::NotFound.response(request_id_value),
        Err(error) => {
            tracing::warn!(%error, %beacon_id, "Beacon admin-state lookup failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    if matches!(payload.status, AdminProfileState::Active)
        && (joined_at.is_none() || !active || !verified || !accepts_outreach || do_not_contact)
    {
        return BeaconSignalError::Conflict.response(request_id_value);
    }
    let update = match payload.status {
        AdminProfileState::Active => sqlx::query(
            "UPDATE viryaos_beacon_signal_profiles SET status='active',paused_at=NULL,revoked_at=NULL,invite_token_hash=NULL,invite_expires_at=NULL,updated_at=now() WHERE workspace_id=$1 AND beacon_id=$2",
        )
        .bind(workspace_id)
        .bind(beacon_id)
        .execute(&mut *tx)
        .await,
        AdminProfileState::Paused => sqlx::query(
            "UPDATE viryaos_beacon_signal_profiles SET status='paused',paused_at=now(),invite_token_hash=NULL,invite_expires_at=NULL,updated_at=now() WHERE workspace_id=$1 AND beacon_id=$2",
        )
        .bind(workspace_id)
        .bind(beacon_id)
        .execute(&mut *tx)
        .await,
        AdminProfileState::Revoked => sqlx::query(
            "UPDATE viryaos_beacon_signal_profiles SET status='revoked',revoked_at=now(),paused_at=NULL,invite_token_hash=NULL,invite_expires_at=NULL,updated_at=now() WHERE workspace_id=$1 AND beacon_id=$2",
        )
        .bind(workspace_id)
        .bind(beacon_id)
        .execute(&mut *tx)
        .await,
    };
    if let Err(error) = update {
        tracing::warn!(%error, %beacon_id, "Beacon admin-state profile update failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    if matches!(payload.status, AdminProfileState::Revoked) {
        for result in [
            sqlx::query(
                "UPDATE viryaos_beacon_signal_sessions SET revoked_at=COALESCE(revoked_at,now()) WHERE workspace_id=$1 AND beacon_id=$2 AND revoked_at IS NULL",
            )
            .bind(workspace_id)
            .bind(beacon_id)
            .execute(&mut *tx)
            .await,
            sqlx::query(
                r#"
                UPDATE fan_push_endpoints endpoint
                SET active=false,invalidated_at=COALESCE(invalidated_at,now()),updated_at=now()
                WHERE endpoint.workspace_id=$1 AND endpoint.audience_kind='beacon' AND endpoint.active
                  AND endpoint.principal_hash IN (
                      SELECT token_hash FROM viryaos_beacon_signal_sessions
                      WHERE workspace_id=$1 AND beacon_id=$2
                  )
                "#,
            )
            .bind(workspace_id)
            .bind(beacon_id)
            .execute(&mut *tx)
            .await,
        ] {
            if let Err(error) = result {
                tracing::warn!(%error, %beacon_id, "Beacon admin-state revocation cleanup failed");
                return BeaconSignalError::Unavailable.response(request_id_value);
            }
        }
    }
    let status = payload.status.as_str();
    let event_payload = serde_json::json!({"beacon_id":beacon_id,"status":status});
    if let Err(error) = sqlx::query(
        "INSERT INTO outbox_events (workspace_id,event_type,event_version,payload,request_id) VALUES ($1,'viryaos.beacon.signal_state_changed',1,$2,$3)",
    )
    .bind(workspace_id)
    .bind(event_payload)
    .bind(request_id(&headers))
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, %beacon_id, "Beacon admin-state outbox write failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    if let Err(error) = tx.commit().await {
        tracing::warn!(%error, %beacon_id, "Beacon admin-state transaction failed to commit");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(AdminProfileStateResponse { beacon_id, status }),
    )
        .into_response()
}

pub async fn admin_resolve_press_request(
    State(state): State<crate::AppState>,
    Path(press_request_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<ResolvePressRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    let resolution_note = match clean_optional_text(payload.resolution_note, 2000) {
        Some(value) => value,
        None => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Beacon press resolution transaction failed to start");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let updated = sqlx::query_as::<_, (Uuid, Option<Uuid>, String)>(
        r#"
        UPDATE viryaos_beacon_press_requests
        SET status=$3,resolved_at=now(),resolution_note=$4,updated_at=now()
        WHERE workspace_id=$1 AND id=$2 AND status='open'
        RETURNING beacon_id,event_id,request_kind
        "#,
    )
    .bind(workspace_id)
    .bind(press_request_id)
    .bind(payload.status.as_str())
    .bind(&resolution_note)
    .fetch_optional(&mut *tx)
    .await;
    let (beacon_id, event_id, request_kind) = match updated {
        Ok(Some(value)) => value,
        Ok(None) => return BeaconSignalError::Conflict.response(request_id_value),
        Err(error) => {
            tracing::warn!(%error, %press_request_id, "Beacon press request resolution failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let event_payload = serde_json::json!({
        "request_id":press_request_id,
        "beacon_id":beacon_id,
        "event_id":event_id,
        "request_kind":request_kind,
        "status":payload.status.as_str(),
        "resolution_note":resolution_note,
    });
    if let Err(error) = sqlx::query(
        "INSERT INTO outbox_events (workspace_id,event_type,event_version,payload,request_id) VALUES ($1,'viryaos.beacon.press_request_resolved',1,$2,$3)",
    )
    .bind(workspace_id)
    .bind(event_payload)
    .bind(request_id(&headers))
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, %press_request_id, "Beacon press resolution outbox write failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    if let Err(error) = tx.commit().await {
        tracing::warn!(%error, %press_request_id, "Beacon press resolution transaction failed to commit");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(ResolvePressResponse {
            request_id: press_request_id,
            status: payload.status.as_str(),
        }),
    )
        .into_response()
}

pub async fn admin_press_assets(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let rows = sqlx::query_as::<_, AdminPressAssetView>(
        r#"
        SELECT asset.id,asset.event_id,event.title AS event_title,asset.asset_key,
               asset.asset_kind,asset.label_pl,asset.label_en,asset.url,
               asset.sort_order,asset.active,asset.updated_at
        FROM viryaos_beacon_press_assets asset
        LEFT JOIN events event
          ON event.workspace_id=asset.workspace_id AND event.id=asset.event_id
        WHERE asset.workspace_id=$1
        ORDER BY asset.event_id NULLS FIRST,asset.sort_order,asset.asset_key,asset.id
        LIMIT 500
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_all(state.ticketing.pool())
    .await;
    match rows {
        Ok(assets) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(AdminPressAssetsResponse { assets }),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "Beacon press asset list failed");
            BeaconSignalError::Unavailable.response(request_id_value)
        }
    }
}

pub async fn admin_upsert_press_asset(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<UpsertPressAssetRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    let asset_key = payload.asset_key.trim().to_owned();
    let asset_kind = payload.asset_kind.trim().to_owned();
    let label_pl = payload.label_pl.trim().to_owned();
    let label_en = payload.label_en.trim().to_owned();
    let url = payload.url.trim().to_owned();
    if !valid_asset_key(&asset_key)
        || !valid_asset_kind(&asset_kind)
        || label_pl.is_empty()
        || label_en.is_empty()
        || label_pl.chars().count() > 120
        || label_en.chars().count() > 120
        || !(0..=10000).contains(&payload.sort_order)
        || !valid_press_url(&url)
    {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    if let Some(event_id) = payload.event_id {
        let exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM events WHERE workspace_id=$1 AND id=$2)",
        )
        .bind(workspace_id)
        .bind(event_id)
        .fetch_one(state.ticketing.pool())
        .await;
        match exists {
            Ok(true) => {}
            Ok(false) => return BeaconSignalError::NotFound.response(request_id_value),
            Err(error) => {
                tracing::warn!(%error, %event_id, "Beacon press asset event lookup failed");
                return BeaconSignalError::Unavailable.response(request_id_value);
            }
        }
    }
    let asset_id = payload.asset_id.unwrap_or_else(Uuid::now_v7);
    let result = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO viryaos_beacon_press_assets
            (id,workspace_id,event_id,asset_key,asset_kind,label_pl,label_en,url,sort_order,active)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
        ON CONFLICT (workspace_id,asset_key,event_id) DO UPDATE SET
            asset_kind=EXCLUDED.asset_kind,label_pl=EXCLUDED.label_pl,label_en=EXCLUDED.label_en,
            url=EXCLUDED.url,sort_order=EXCLUDED.sort_order,active=EXCLUDED.active,updated_at=now()
        RETURNING id
        "#,
    )
    .bind(asset_id)
    .bind(workspace_id)
    .bind(payload.event_id)
    .bind(asset_key)
    .bind(asset_kind)
    .bind(label_pl)
    .bind(label_en)
    .bind(url)
    .bind(payload.sort_order)
    .bind(payload.active)
    .fetch_one(state.ticketing.pool())
    .await;
    match result {
        Ok(asset_id) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(UpsertPressAssetResponse { asset_id }),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "Beacon press asset upsert failed");
            BeaconSignalError::Unavailable.response(request_id_value)
        }
    }
}

pub async fn admin_engagements(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Query(query): Query<AdminEngagementQuery>,
) -> Response {
    let request_id_value = request_id(&headers);
    if query
        .status
        .as_deref()
        .is_some_and(|value| !valid_engagement_status(value))
    {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    let rows = sqlx::query_as::<_, AdminEngagementView>(
        r#"
        SELECT engagement.beacon_id,beacon.display_name,beacon.beacon_kind,
               engagement.event_id,event.title AS event_title,event.slug AS event_slug,
               engagement.status,engagement.help_kind,engagement.help_details,
               engagement.notification_count,
               COALESCE(coverage.coverage_count,0)::bigint AS coverage_count,
               engagement.last_notified_at,engagement.updated_at
        FROM viryaos_beacon_signal_event_engagements engagement
        JOIN viryaos_beacons beacon
          ON beacon.workspace_id=engagement.workspace_id AND beacon.id=engagement.beacon_id
        JOIN events event
          ON event.workspace_id=engagement.workspace_id AND event.id=engagement.event_id
        LEFT JOIN (
            SELECT workspace_id,beacon_id,event_id,count(*)::bigint AS coverage_count
            FROM viryaos_beacon_signal_coverage
            GROUP BY workspace_id,beacon_id,event_id
        ) coverage
          ON coverage.workspace_id=engagement.workspace_id
         AND coverage.beacon_id=engagement.beacon_id AND coverage.event_id=engagement.event_id
        WHERE engagement.workspace_id=$1 AND ($2::text IS NULL OR engagement.status=$2)
        ORDER BY engagement.updated_at DESC,engagement.event_id,engagement.beacon_id
        LIMIT 300
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(query.status.as_deref())
    .fetch_all(state.ticketing.pool())
    .await;
    match rows {
        Ok(engagements) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(AdminEngagementsResponse { engagements }),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "Beacon engagement admin list failed");
            BeaconSignalError::Unavailable.response(request_id_value)
        }
    }
}

pub async fn admin_coverage(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let request_id_value = request_id(&headers);
    let rows = sqlx::query_as::<_, AdminCoverageView>(
        r#"
        SELECT coverage.id,coverage.beacon_id,beacon.display_name,coverage.event_id,
               event.title AS event_title,coverage.coverage_kind,coverage.url,coverage.title,
               coverage.created_at
        FROM viryaos_beacon_signal_coverage coverage
        JOIN viryaos_beacons beacon
          ON beacon.workspace_id=coverage.workspace_id AND beacon.id=coverage.beacon_id
        JOIN events event
          ON event.workspace_id=coverage.workspace_id AND event.id=coverage.event_id
        WHERE coverage.workspace_id=$1
        ORDER BY coverage.created_at DESC,coverage.id DESC
        LIMIT 300
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_all(state.ticketing.pool())
    .await;
    match rows {
        Ok(coverage) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(AdminCoverageResponse { coverage }),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "Beacon coverage admin list failed");
            BeaconSignalError::Unavailable.response(request_id_value)
        }
    }
}

pub async fn admin_press_requests(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let result = sqlx::query_as::<_, AdminPressRequestView>(
        r#"
        SELECT request.id, request.beacon_id, beacon.display_name, beacon.beacon_kind,
               request.event_id, event.title AS event_title, request.request_kind,
               request.details, request.status, request.resolution_note,
               request.created_at, request.resolved_at
        FROM viryaos_beacon_press_requests request
        JOIN viryaos_beacons beacon
          ON beacon.workspace_id=request.workspace_id AND beacon.id=request.beacon_id
        LEFT JOIN events event
          ON event.workspace_id=request.workspace_id AND event.id=request.event_id
        WHERE request.workspace_id=$1
        ORDER BY CASE request.status WHEN 'open' THEN 0 ELSE 1 END,
                 request.created_at DESC, request.id DESC
        LIMIT 100
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_all(state.ticketing.pool())
    .await;
    match result {
        Ok(requests) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(AdminPressRequestsResponse { requests }),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "Beacon press-request list failed");
            BeaconSignalError::Unavailable.response(request_id_value)
        }
    }
}
