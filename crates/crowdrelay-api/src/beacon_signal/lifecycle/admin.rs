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
    let brand = match crowdrelay_infra::tenant_settings::TenantSettingsRepository::new(
        state.database.clone(),
    )
    .brand_settings(workspace_id)
    .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Tenant brand settings lookup failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let response = match mint_invite_batch_tx(
        &mut tx,
        &brand,
        workspace_id,
        &beacon_ids,
        payload.ttl_days,
        payload.radius_km,
        &locale,
        None,
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
        FROM beacons beacon
        LEFT JOIN cities city ON city.id=beacon.city_id
        LEFT JOIN beacon_signal_profiles profile
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
            Json(AdminCandidatesResponse {
                candidates: fold_candidates(candidates),
            }),
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
            FROM beacon_signal_sessions
            WHERE revoked_at IS NULL AND expires_at > now()
            GROUP BY workspace_id,beacon_id
        ), endpoint_counts AS (
            SELECT session.workspace_id,session.beacon_id,count(DISTINCT endpoint.id)::bigint AS active_push_endpoints
            FROM beacon_signal_sessions session
            JOIN fan_push_endpoints endpoint
              ON endpoint.workspace_id=session.workspace_id
             AND endpoint.audience_kind='beacon'
             AND endpoint.principal_hash=session.token_hash
             AND endpoint.active AND endpoint.invalidated_at IS NULL
            WHERE session.revoked_at IS NULL AND session.expires_at > now()
            GROUP BY session.workspace_id,session.beacon_id
        ), request_counts AS (
            SELECT workspace_id,beacon_id,count(*)::bigint AS open_press_requests
            FROM beacon_press_requests WHERE status='open'
            GROUP BY workspace_id,beacon_id
        ), engagement_counts AS (
            SELECT workspace_id,beacon_id,count(*)::bigint AS active_engagements
            FROM beacon_signal_event_engagements
            WHERE status NOT IN ('completed','declined')
            GROUP BY workspace_id,beacon_id
        ), coverage_counts AS (
            SELECT workspace_id,beacon_id,count(*)::bigint AS coverage_count
            FROM beacon_signal_coverage GROUP BY workspace_id,beacon_id
        )
        SELECT beacon.id AS beacon_id,beacon.display_name,beacon.beacon_kind,beacon.contact_email,
               city.name AS city,
               COALESCE(profile.status,'unverified') AS status,
               COALESCE(profile.radius_km,0) AS radius_km,
               COALESCE(profile.locale,'') AS locale,
               COALESCE(profile.nearby_gigs_enabled,false) AS nearby_gigs_enabled,
               COALESCE(profile.invite_count,0) AS invite_count,
               profile.last_invited_at,
               profile.invite_expires_at,
               profile.joined_at,
               profile.last_seen_at,
               COALESCE(session_counts.active_sessions,0)::bigint AS active_sessions,
               COALESCE(endpoint_counts.active_push_endpoints,0)::bigint AS active_push_endpoints,
               COALESCE(request_counts.open_press_requests,0)::bigint AS open_press_requests,
               COALESCE(engagement_counts.active_engagements,0)::bigint AS active_engagements,
               COALESCE(coverage_counts.coverage_count,0)::bigint AS coverage_count,
               beacon.relevance_basis_points,beacon.relationship_score,
               beacon.destination_url,beacon.verified,beacon.accepts_outreach,beacon.do_not_contact
        FROM beacons beacon
        LEFT JOIN beacon_signal_profiles profile
          ON profile.workspace_id=beacon.workspace_id AND profile.beacon_id=beacon.id
        LEFT JOIN cities city ON city.id=beacon.city_id
        LEFT JOIN session_counts ON session_counts.workspace_id=beacon.workspace_id AND session_counts.beacon_id=beacon.id
        LEFT JOIN endpoint_counts ON endpoint_counts.workspace_id=beacon.workspace_id AND endpoint_counts.beacon_id=beacon.id
        LEFT JOIN request_counts ON request_counts.workspace_id=beacon.workspace_id AND request_counts.beacon_id=beacon.id
        LEFT JOIN engagement_counts ON engagement_counts.workspace_id=beacon.workspace_id AND engagement_counts.beacon_id=beacon.id
        LEFT JOIN coverage_counts ON coverage_counts.workspace_id=beacon.workspace_id AND coverage_counts.beacon_id=beacon.id
        WHERE beacon.workspace_id=$1 AND beacon.active
        ORDER BY CASE COALESCE(profile.status,'unverified')
                   WHEN 'active' THEN 0 WHEN 'invited' THEN 1
                   WHEN 'paused' THEN 2 WHEN 'revoked' THEN 3 ELSE 4 END,
                 (beacon.contact_email IS NOT NULL) DESC,
                 beacon.relevance_basis_points DESC,beacon.relationship_score DESC,beacon.display_name,beacon.id
        LIMIT 500
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_all(state.ticketing.pool())
    .await;
    match profiles {
        Ok(profiles) => {
            let profiles = fold_profiles(profiles);
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

/// The roster is entities, not rows. The same person arrives under several
/// kinds — a radio that is also a scene partner is one human, and the
/// per-kind identity keys that keep contact routes honest also let those
/// facets stand side by side in the list: three identical names, three
/// kinds, one person. Rows sharing a normalized (name, city) fold into the
/// entity they describe when at most one of them carries a contact identity
/// (an email or a destination URL). Two rows with the same name and
/// different addresses stay apart — that is a second way to reach the
/// entity, not a duplicate.
///
/// The folded row keeps the facet an invite would mint against — the
/// emailable, consented, strongest facet — while the kind column names
/// every hat the entity wears ("community · creator"), the truth the
/// per-row display could not say. Suppression dominates: one revoked or
/// paused facet makes the entity read revoked or paused, because the
/// operator set that state and no sibling stub may quietly reopen it.
fn fold_profiles(profiles: Vec<AdminProfileView>) -> Vec<AdminProfileView> {
    use std::collections::BTreeMap;

    /// Suppression-first ranking for folded status: an operator-set state
    /// outranks a flow state, and any reached state outranks unverified.
    fn status_rank(status: &str) -> u8 {
        match status {
            "revoked" => 0,
            "paused" => 1,
            "active" => 2,
            "invited" => 3,
            _ => 4,
        }
    }
    /// Display order — the ranking the dashboard already sorted by.
    fn display_rank(status: &str) -> u8 {
        match status {
            "active" => 0,
            "invited" => 1,
            "paused" => 2,
            "revoked" => 3,
            _ => 4,
        }
    }
    let key_of = |p: &AdminProfileView| {
        let name = p
            .display_name
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        let city = p
            .city
            .clone()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        (name, city)
    };
    let anchor_of = |p: &AdminProfileView| {
        p.contact_email
            .as_deref()
            .map(str::trim)
            .map(str::to_lowercase)
            .or_else(|| {
                p.destination_url
                    .as_deref()
                    .map(str::trim)
                    .map(str::to_lowercase)
            })
    };
    // The facet a mint would pick: emailable + consented first, then simply
    // reachable — a folded entity whose only address sits on an unverified
    // facet still names that address, otherwise the row would read "no
    // email" over a contact route that exists. Then the strongest signal,
    // stable on id.
    let strength = |p: &AdminProfileView| {
        (
            p.contact_email.is_some() && p.verified && p.accepts_outreach && !p.do_not_contact,
            p.contact_email.is_some(),
            p.relevance_basis_points,
            p.relationship_score,
            p.invite_count,
            std::cmp::Reverse(p.beacon_id),
        )
    };

    let mut groups: BTreeMap<(String, String), Vec<AdminProfileView>> = BTreeMap::new();
    for profile in profiles {
        groups.entry(key_of(&profile)).or_default().push(profile);
    }
    let mut folded: Vec<AdminProfileView> = Vec::with_capacity(groups.len());
    for (_, mut rows) in groups {
        let anchors: std::collections::BTreeSet<String> =
            rows.iter().filter_map(&anchor_of).collect();
        if rows.len() == 1 || anchors.len() > 1 {
            folded.append(&mut rows);
            continue;
        }
        rows.sort_by_key(|p| std::cmp::Reverse(strength(p)));
        let mut kinds: Vec<String> = rows.iter().map(|p| p.beacon_kind.clone()).collect();
        kinds.sort();
        kinds.dedup();
        // The row owning the strictest status supplies the status-bound
        // fields (expiry, radius, locale, nearby flag).
        let Some(state_row) = rows.iter().min_by(|a, b| {
            status_rank(&a.status)
                .cmp(&status_rank(&b.status))
                .then(a.beacon_id.cmp(&b.beacon_id))
        }) else {
            continue;
        };
        let status = state_row.status.clone();
        let radius_km = state_row.radius_km;
        let locale = state_row.locale.clone();
        let nearby_gigs_enabled = state_row.nearby_gigs_enabled;
        let invite_expires_at = state_row.invite_expires_at;
        let max_opt = |f: &dyn Fn(&AdminProfileView) -> Option<OffsetDateTime>| {
            rows.iter().filter_map(f).max()
        };
        // Contact-identity and consent fields come from the canonical facet —
        // the row the mint would act on. `any()` here would lie in both
        // directions: a sibling stub's missing email would still show an
        // address the mint cannot use, and a sibling's do-not-contact would
        // grey out a row the mint happily mails.
        let Some(canonical) = rows.first() else {
            continue;
        };
        folded.push(AdminProfileView {
            beacon_id: canonical.beacon_id,
            display_name: canonical.display_name.clone(),
            beacon_kind: kinds.join(" · "),
            contact_email: canonical.contact_email.clone(),
            city: canonical.city.clone(),
            status,
            radius_km,
            locale,
            nearby_gigs_enabled,
            invite_count: rows.iter().map(|p| p.invite_count).sum(),
            last_invited_at: max_opt(&|p| p.last_invited_at),
            invite_expires_at,
            // `joined_at` gates the Resume button, and Resume posts the
            // canonical id — the group max would render a button the
            // upstream refuses when only a sibling ever joined.
            joined_at: canonical.joined_at,
            last_seen_at: max_opt(&|p| p.last_seen_at),
            active_sessions: rows.iter().map(|p| p.active_sessions).sum(),
            active_push_endpoints: rows.iter().map(|p| p.active_push_endpoints).sum(),
            open_press_requests: rows.iter().map(|p| p.open_press_requests).sum(),
            active_engagements: rows.iter().map(|p| p.active_engagements).sum(),
            coverage_count: rows.iter().map(|p| p.coverage_count).sum(),
            relevance_basis_points: canonical.relevance_basis_points,
            relationship_score: canonical.relationship_score,
            destination_url: canonical.destination_url.clone(),
            verified: canonical.verified,
            accepts_outreach: canonical.accepts_outreach,
            do_not_contact: canonical.do_not_contact,
        });
    }
    folded.sort_by(|a, b| {
        display_rank(&a.status)
            .cmp(&display_rank(&b.status))
            .then(b.contact_email.is_some().cmp(&a.contact_email.is_some()))
            .then(b.relevance_basis_points.cmp(&a.relevance_basis_points))
            .then(b.relationship_score.cmp(&a.relationship_score))
            .then(a.display_name.cmp(&b.display_name))
            .then(a.beacon_id.cmp(&b.beacon_id))
    });
    folded
}

/// One letter per address. The candidate list is keyed by contact route,
/// so the same address under two kinds is one letter, not two — the mint
/// dedupes on the email, and the list now reads the same way.
fn fold_candidates(candidates: Vec<AdminCandidateView>) -> Vec<AdminCandidateView> {
    use std::collections::BTreeMap;

    fn status_rank(status: Option<&str>) -> u8 {
        match status {
            Some("revoked") => 0,
            Some("paused") => 1,
            Some("active") => 2,
            Some("invited") => 3,
            _ => 4,
        }
    }
    let mut groups: BTreeMap<String, Vec<AdminCandidateView>> = BTreeMap::new();
    for candidate in candidates {
        let key = candidate.contact_email.trim().to_lowercase();
        groups.entry(key).or_default().push(candidate);
    }
    let mut folded: Vec<AdminCandidateView> = Vec::with_capacity(groups.len());
    for (_, mut rows) in groups {
        if rows.len() == 1 {
            folded.append(&mut rows);
            continue;
        }
        rows.sort_by(|a, b| {
            (
                b.relevance_basis_points,
                b.relationship_score,
                b.invite_count,
                std::cmp::Reverse(b.beacon_id),
            )
                .cmp(&(
                    a.relevance_basis_points,
                    a.relationship_score,
                    a.invite_count,
                    std::cmp::Reverse(a.beacon_id),
                ))
        });
        let mut kinds: Vec<String> = rows.iter().map(|c| c.beacon_kind.clone()).collect();
        kinds.sort();
        kinds.dedup();
        let signal_status = rows
            .iter()
            .min_by(|a, b| {
                status_rank(a.signal_status.as_deref())
                    .cmp(&status_rank(b.signal_status.as_deref()))
            })
            .and_then(|c| c.signal_status.clone());
        let invite_count = rows.iter().map(|c| c.invite_count).sum();
        let last_invited_at = rows.iter().filter_map(|c| c.last_invited_at).max();
        let mut canonical = rows.remove(0);
        canonical.beacon_kind = kinds.join(" · ");
        canonical.signal_status = signal_status;
        canonical.invite_count = invite_count;
        canonical.last_invited_at = last_invited_at;
        folded.push(canonical);
    }
    folded.sort_by(|a, b| {
        (b.relevance_basis_points, b.relationship_score)
            .cmp(&(a.relevance_basis_points, a.relationship_score))
            .then(a.display_name.cmp(&b.display_name))
            .then(a.beacon_id.cmp(&b.beacon_id))
    });
    folded
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
        FROM beacon_signal_profiles profile
        JOIN beacons beacon
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
            "UPDATE beacon_signal_profiles SET status='active',paused_at=NULL,revoked_at=NULL,invite_token_hash=NULL,invite_expires_at=NULL,updated_at=now() WHERE workspace_id=$1 AND beacon_id=$2",
        )
        .bind(workspace_id)
        .bind(beacon_id)
        .execute(&mut *tx)
        .await,
        AdminProfileState::Paused => sqlx::query(
            "UPDATE beacon_signal_profiles SET status='paused',paused_at=now(),invite_token_hash=NULL,invite_expires_at=NULL,updated_at=now() WHERE workspace_id=$1 AND beacon_id=$2",
        )
        .bind(workspace_id)
        .bind(beacon_id)
        .execute(&mut *tx)
        .await,
        AdminProfileState::Revoked => sqlx::query(
            "UPDATE beacon_signal_profiles SET status='revoked',revoked_at=now(),paused_at=NULL,invite_token_hash=NULL,invite_expires_at=NULL,updated_at=now() WHERE workspace_id=$1 AND beacon_id=$2",
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
                "UPDATE beacon_signal_sessions SET revoked_at=COALESCE(revoked_at,now()) WHERE workspace_id=$1 AND beacon_id=$2 AND revoked_at IS NULL",
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
                      SELECT token_hash FROM beacon_signal_sessions
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
        "INSERT INTO outbox_events (workspace_id,event_type,event_version,payload,request_id) VALUES ($1,'crowdrelay.beacon.signal_state_changed',1,$2,$3)",
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
        UPDATE beacon_press_requests
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
        "INSERT INTO outbox_events (workspace_id,event_type,event_version,payload,request_id) VALUES ($1,'crowdrelay.beacon.press_request_resolved',1,$2,$3)",
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
        FROM beacon_press_assets asset
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
        INSERT INTO beacon_press_assets
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
        FROM beacon_signal_event_engagements engagement
        JOIN beacons beacon
          ON beacon.workspace_id=engagement.workspace_id AND beacon.id=engagement.beacon_id
        JOIN events event
          ON event.workspace_id=engagement.workspace_id AND event.id=engagement.event_id
        LEFT JOIN (
            SELECT workspace_id,beacon_id,event_id,count(*)::bigint AS coverage_count
            FROM beacon_signal_coverage
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
        FROM beacon_signal_coverage coverage
        JOIN beacons beacon
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
        FROM beacon_press_requests request
        JOIN beacons beacon
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

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str, kind: &str, city: Option<&str>, id: u128) -> AdminProfileView {
        AdminProfileView {
            beacon_id: Uuid::from_u128(id),
            display_name: name.to_string(),
            beacon_kind: kind.to_string(),
            contact_email: None,
            city: city.map(str::to_string),
            status: "unverified".to_string(),
            radius_km: 0,
            locale: String::new(),
            nearby_gigs_enabled: false,
            invite_count: 0,
            last_invited_at: None,
            invite_expires_at: None,
            joined_at: None,
            last_seen_at: None,
            active_sessions: 0,
            active_push_endpoints: 0,
            open_press_requests: 0,
            active_engagements: 0,
            coverage_count: 0,
            relevance_basis_points: 0,
            relationship_score: 0,
            destination_url: None,
            verified: true,
            accepts_outreach: true,
            do_not_contact: false,
        }
    }

    fn candidate(name: &str, kind: &str, email: &str, id: u128) -> AdminCandidateView {
        AdminCandidateView {
            beacon_id: Uuid::from_u128(id),
            display_name: name.to_string(),
            beacon_kind: kind.to_string(),
            contact_email: email.to_string(),
            city: None,
            relevance_basis_points: 0,
            relationship_score: 0,
            signal_status: None,
            invite_count: 0,
            last_invited_at: None,
        }
    }

    #[test]
    fn folds_kind_facets_of_one_entity() {
        let mut a = profile(
            "Bydgoszcz radio, słuchaj online",
            "creator",
            Some("Bydgoszcz"),
            1,
        );
        a.relevance_basis_points = 9000;
        let b = profile(
            "Bydgoszcz radio, słuchaj online",
            "community",
            Some("Bydgoszcz"),
            2,
        );
        let c = profile(
            "bydgoszcz  radio, słuchaj online",
            "creator",
            Some("Bydgoszcz"),
            3,
        );
        let folded = fold_profiles(vec![a, b, c]);
        assert_eq!(folded.len(), 1);
        assert_eq!(
            folded[0].beacon_id,
            Uuid::from_u128(1),
            "strongest facet keeps the id"
        );
        assert_eq!(folded[0].beacon_kind, "community · creator");
    }

    #[test]
    fn keeps_distinct_contact_routes_apart() {
        let mut a = profile("Radio", "radio", Some("Bydgoszcz"), 1);
        a.contact_email = Some("a@example.com".into());
        let mut b = profile("Radio", "radio", Some("Bydgoszcz"), 2);
        b.contact_email = Some("b@example.com".into());
        assert_eq!(fold_profiles(vec![a, b]).len(), 2);
    }

    #[test]
    fn stub_adopts_into_the_emailed_facet() {
        let mut a = profile("Zine", "zine", None, 1);
        a.contact_email = Some("zine@example.com".into());
        a.relevance_basis_points = 5000;
        let b = profile("Zine", "collective", None, 2);
        let folded = fold_profiles(vec![b, a]);
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].beacon_id, Uuid::from_u128(1));
        assert_eq!(folded[0].contact_email.as_deref(), Some("zine@example.com"));
        assert_eq!(folded[0].beacon_kind, "collective · zine");
    }

    #[test]
    fn distinct_destinations_stay_apart() {
        let mut a = profile("Shop", "shop", Some("Gdańsk"), 1);
        a.destination_url = Some("https://a.example".into());
        let mut b = profile("Shop", "shop", Some("Gdańsk"), 2);
        b.destination_url = Some("https://b.example".into());
        assert_eq!(fold_profiles(vec![a, b]).len(), 2);
    }

    #[test]
    fn an_operator_state_outranks_a_flow_state() {
        let mut a = profile("Collective", "collective", None, 1);
        a.status = "revoked".to_string();
        let mut b = profile("Collective", "community", None, 2);
        b.status = "invited".to_string();
        let folded = fold_profiles(vec![b, a]);
        assert_eq!(folded[0].status, "revoked");
    }

    #[test]
    fn different_cities_are_different_entities() {
        let a = profile("City Radio", "radio", Some("Bydgoszcz"), 1);
        let b = profile("City Radio", "radio", Some("Toruń"), 2);
        assert_eq!(fold_profiles(vec![a, b]).len(), 2);
    }

    #[test]
    fn folds_candidates_by_address() {
        let a = candidate("Es KA", "radio", "Host@example.com", 1);
        let b = candidate("Es KA", "community", "host@example.com", 2);
        let folded = fold_candidates(vec![a, b]);
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].beacon_kind, "community · radio");
    }
}
