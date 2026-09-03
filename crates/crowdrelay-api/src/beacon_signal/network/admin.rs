use super::*;

pub async fn admin_beacon_network(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
) -> Response {
    let request_id_value = request_id(&headers);
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let discovery_runs = match sqlx::query_as::<_, DiscoveryRunView>(
        r#"
        SELECT id,country_code,target_count,status,discovered_count,report_filename,
               report_sha256,requested_at,completed_at,failure_kind
        FROM viryaos_beacon_network_discovery_runs
        WHERE workspace_id=$1
        ORDER BY requested_at DESC,id DESC
        LIMIT 25
        "#,
    )
    .bind(workspace_id)
    .fetch_all(state.ticketing.pool())
    .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Latarnik network discovery listing failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let pending_candidates = match sqlx::query_as::<_, DiscoveredBeaconView>(
        r#"
        SELECT id,display_name,beacon_kind,contact_email,destination_url,source_url,
               verified,accepts_outreach,do_not_contact,metadata
        FROM viryaos_beacons
        WHERE workspace_id=$1
          -- Discovery is one way onto this list; importing researched
          -- contacts is the other. Both arrive unverified and both need the
          -- same operator approval before the invite pipeline will touch them,
          -- so both belong in the same queue.
          AND (metadata ? 'network_discovery_run_id' OR metadata ? 'imported_from')
          AND active AND NOT do_not_contact
          AND (NOT verified OR NOT accepts_outreach)
        ORDER BY created_at DESC,id DESC
        LIMIT 300
        "#,
    )
    .bind(workspace_id)
    .fetch_all(state.ticketing.pool())
    .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Latarnik network candidate listing failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let approved_candidates = match sqlx::query_as::<_, DiscoveredBeaconView>(
        r#"
        SELECT beacon.id,beacon.display_name,beacon.beacon_kind,beacon.contact_email,
               beacon.destination_url,beacon.source_url,beacon.verified,beacon.accepts_outreach,
               beacon.do_not_contact,beacon.metadata
        FROM viryaos_beacons beacon
        LEFT JOIN viryaos_beacon_signal_profiles profile
          ON profile.workspace_id=beacon.workspace_id AND profile.beacon_id=beacon.id
        WHERE beacon.workspace_id=$1
          AND (beacon.metadata ? 'network_discovery_run_id' OR beacon.metadata ? 'imported_from')
          AND beacon.active AND beacon.verified AND beacon.accepts_outreach AND NOT beacon.do_not_contact
          AND beacon.contact_email IS NOT NULL
          AND COALESCE(profile.status,'') <> 'active'
          -- A never-invited beacon has no profile row at all. Three-valued logic
          -- turns the bare NOT(...) into NULL there and silently drops exactly the
          -- first-wave candidates this flow exists to reach, so fold NULL to false.
          AND NOT COALESCE(profile.status='invited' AND profile.invite_expires_at > now(), false)
        ORDER BY beacon.updated_at DESC,beacon.id DESC
        LIMIT 300
        "#,
    )
    .bind(workspace_id)
    .fetch_all(state.ticketing.pool())
    .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Latarnik network approved candidate listing failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let invite_jobs = match sqlx::query_as::<_, InviteJobView>(
        r#"
        WITH recent_jobs AS (
            SELECT id,workspace_id,status,beacon_ids,ttl_days,radius_km,locale,claimed_by,
                   claimed_at,claim_expires_at,reported_at,provider_summary,created_at
            FROM viryaos_beacon_invite_delivery_jobs
            WHERE workspace_id=$1
            ORDER BY created_at DESC,id DESC
            LIMIT 50
        ), session_metrics AS (
            SELECT session.source_invite_job_id AS job_id,
                   count(DISTINCT session.beacon_id)::bigint AS exchanged_count,
                   count(DISTINCT session.beacon_id) FILTER (WHERE session.client_kind='web')::bigint AS web_count,
                   count(DISTINCT session.beacon_id) FILTER (WHERE session.client_kind='android')::bigint AS android_count,
                   count(DISTINCT session.beacon_id) FILTER (WHERE session.client_kind='ios')::bigint AS ios_count,
                   count(DISTINCT session.beacon_id) FILTER (
                       WHERE session.revoked_at IS NULL AND session.expires_at > now()
                         AND profile.status='active'
                   )::bigint AS active_count
            FROM viryaos_beacon_signal_sessions session
            JOIN recent_jobs job
              ON job.workspace_id=session.workspace_id AND job.id=session.source_invite_job_id
            JOIN viryaos_beacon_signal_profiles profile
              ON profile.workspace_id=session.workspace_id AND profile.beacon_id=session.beacon_id
            GROUP BY session.source_invite_job_id
        ), push_metrics AS (
            SELECT session.source_invite_job_id AS job_id,
                   count(DISTINCT session.beacon_id)::bigint AS push_enabled_count
            FROM viryaos_beacon_signal_sessions session
            JOIN recent_jobs job
              ON job.workspace_id=session.workspace_id AND job.id=session.source_invite_job_id
            JOIN fan_push_endpoints endpoint
              ON endpoint.workspace_id=session.workspace_id
             AND endpoint.principal_hash=session.token_hash
             AND endpoint.audience_kind='beacon'
             AND endpoint.active AND endpoint.invalidated_at IS NULL
            WHERE session.revoked_at IS NULL AND session.expires_at > now()
            GROUP BY session.source_invite_job_id
        ), engagement_metrics AS (
            SELECT session.source_invite_job_id AS job_id,
                   count(DISTINCT engagement.beacon_id) FILTER (
                       WHERE engagement.status IN ('helping','completed')
                         AND engagement.updated_at >= session.created_at
                         AND engagement.updated_at < session.expires_at
                         AND (session.revoked_at IS NULL OR engagement.updated_at < session.revoked_at)
                   )::bigint AS helping_count
            FROM viryaos_beacon_signal_sessions session
            JOIN recent_jobs job
              ON job.workspace_id=session.workspace_id AND job.id=session.source_invite_job_id
            JOIN viryaos_beacon_signal_event_engagements engagement
              ON engagement.workspace_id=session.workspace_id
             AND engagement.beacon_id=session.beacon_id
            GROUP BY session.source_invite_job_id
        ), coverage_metrics AS (
            SELECT session.source_invite_job_id AS job_id,
                   count(DISTINCT coverage.beacon_id) FILTER (
                       WHERE coverage.created_at >= session.created_at
                         AND coverage.created_at < session.expires_at
                         AND (session.revoked_at IS NULL OR coverage.created_at < session.revoked_at)
                   )::bigint AS coverage_count
            FROM viryaos_beacon_signal_sessions session
            JOIN recent_jobs job
              ON job.workspace_id=session.workspace_id AND job.id=session.source_invite_job_id
            JOIN viryaos_beacon_signal_coverage coverage
              ON coverage.workspace_id=session.workspace_id
             AND coverage.beacon_id=session.beacon_id
            GROUP BY session.source_invite_job_id
        )
        SELECT job.id,job.status,cardinality(job.beacon_ids)::integer AS beacon_count,
               job.ttl_days,job.radius_km,job.locale,job.claimed_by,job.claimed_at,
               job.claim_expires_at,job.reported_at,job.provider_summary,
               COALESCE(session_metrics.exchanged_count,0)::bigint AS exchanged_count,
               COALESCE(session_metrics.web_count,0)::bigint AS web_count,
               COALESCE(session_metrics.android_count,0)::bigint AS android_count,
               COALESCE(session_metrics.ios_count,0)::bigint AS ios_count,
               COALESCE(session_metrics.active_count,0)::bigint AS active_count,
               COALESCE(push_metrics.push_enabled_count,0)::bigint AS push_enabled_count,
               COALESCE(engagement_metrics.helping_count,0)::bigint AS helping_count,
               COALESCE(coverage_metrics.coverage_count,0)::bigint AS coverage_count,
               job.created_at
        FROM recent_jobs job
        LEFT JOIN session_metrics ON session_metrics.job_id=job.id
        LEFT JOIN push_metrics ON push_metrics.job_id=job.id
        LEFT JOIN engagement_metrics ON engagement_metrics.job_id=job.id
        LEFT JOIN coverage_metrics ON coverage_metrics.job_id=job.id
        ORDER BY job.created_at DESC,job.id DESC
        "#,
    )
    .bind(workspace_id)
    .fetch_all(state.ticketing.pool())
    .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Latarnik network invite-job listing failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    // Researched contacts that have not been imported yet. Counted the same
    // way the import selects, so the number on the button matches what
    // pressing it does.
    let researched_available = sqlx::query_scalar::<_, i64>(
        r#"
        WITH researched AS (
            SELECT id::text AS source_id, contact_email IS NOT NULL AS has_route
            FROM agent_outreach_targets
            WHERE workspace_id=$1 AND status <> 'discarded' AND NOT do_not_contact
            UNION ALL
            SELECT id::text, route_is_published
            FROM viryaos_outreach_candidates
            WHERE workspace_id=$1 AND status IN ('admitted','promoted')
        )
        SELECT count(*)
        FROM researched
        WHERE researched.has_route
          AND NOT EXISTS (
            SELECT 1 FROM viryaos_beacons beacon
            WHERE beacon.workspace_id=$1
              AND beacon.metadata -> 'imported_from' ->> 'source_id' = researched.source_id
          )
        "#,
    )
    .bind(workspace_id)
    .fetch_one(state.ticketing.pool())
    .await
    .unwrap_or_else(|error| {
        // A count that fails should hide the button, not blank the console.
        tracing::warn!(%error, "Latarnik researched-contact count failed");
        0
    });
    private_json(
        StatusCode::OK,
        BeaconNetworkResponse {
            discovery_runs,
            pending_candidates,
            approved_candidates,
            invite_jobs,
            researched_available,
        },
    )
}

pub async fn admin_beacon_network_action(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<AdminNetworkActionRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    if payload.action == "preview_invites" {
        return preview_invites(&state, &headers, payload).await;
    }
    let Some(idempotency_key) = idempotency_key(&headers) else {
        return BeaconSignalError::BadRequest.response(request_id_value);
    };
    match payload.action.as_str() {
        "discover" => request_discovery(&state, &headers, payload, idempotency_key).await,
        "import_researched" => import_researched(&state, &headers, idempotency_key).await,
        "import_submithub" => import_submithub(&state, &headers, payload, idempotency_key).await,
        "approve" => approve_candidate(&state, &headers, payload, idempotency_key).await,
        "queue_invites" => queue_invites(&state, &headers, payload, idempotency_key).await,
        _ => BeaconSignalError::BadRequest.response(request_id_value),
    }
}

/// Turns researched contacts into roster rows awaiting approval.
///
/// The research existed and had nowhere to go: agents proposed targets and the
/// candidate screener admitted routes read from published pages, while the
/// roster held three beacons and the network's own `discover` path had never
/// been run. This is the bridge.
///
/// The write lives in `crowdrelay-infra` — the HTTP layer holds no INSERT —
/// and imported rows land unverified, so they queue behind the same operator
/// approval as anything discovery produces.
async fn import_researched(
    state: &crate::AppState,
    headers: &HeaderMap,
    idempotency_key: String,
) -> Response {
    let request_id_value = request_id(headers);
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    match crowdrelay_infra::beacon_signal::import::import_researched_beacons(
        state.ticketing.pool(),
        workspace_id,
        &idempotency_key,
        request_id_value.as_deref(),
    )
    .await
    {
        Ok(summary) => private_json(
            StatusCode::OK,
            json!({
                "imported": summary.imported,
                "alreadyPresent": summary.already_present,
                "skippedNoRoute": summary.skipped_no_route,
                "skippedDoNotContact": summary.skipped_do_not_contact,
                "considered": summary.considered(),
            }),
        ),
        Err(error) => {
            tracing::warn!(%error, "Latarnik researched beacon import failed");
            BeaconSignalError::Unavailable.response(request_id_value)
        }
    }
}

/// Import SubmitHub curators as unverified beacons.
///
/// The control plane parses the Activity CSV and sends structured rows
/// here. Only curators whose action was `approved` or `shared` are
/// imported — warm contacts who engaged positively. The CSV carries no
/// contact routes, so imported beacons start with no email or URL and
/// the operator enriches them from the SubmitHub chat before approval.
async fn import_submithub(
    state: &crate::AppState,
    headers: &HeaderMap,
    payload: AdminNetworkActionRequest,
    idempotency_key: String,
) -> Response {
    let request_id_value = request_id(headers);
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let Some(csv_rows) = payload.csv_rows.as_deref() else {
        return BeaconSignalError::BadRequest.response(request_id_value);
    };
    if csv_rows.is_empty() || csv_rows.len() > MAX_DISCOVERY_BATCH {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    let inputs: Vec<_> = csv_rows
        .iter()
        .map(
            |row| crowdrelay_infra::beacon_signal::import::SubmithubImportInput {
                outlet: row.outlet.clone(),
                outlet_type: row.outlet_type.clone(),
                action: row.action.clone(),
                song: row.song.clone(),
                country: row.country.clone(),
                feedback: row.feedback.clone(),
                action_timestamp: row.action_timestamp.clone(),
            },
        )
        .collect();
    match crowdrelay_infra::beacon_signal::import::import_submithub_beacons(
        state.ticketing.pool(),
        workspace_id,
        &inputs,
        &idempotency_key,
        request_id_value.as_deref(),
    )
    .await
    {
        Ok(summary) => private_json(
            StatusCode::OK,
            json!({
                "imported": summary.imported,
                "alreadyPresent": summary.already_present,
                "skippedNoRoute": summary.skipped_no_route,
                "skippedDoNotContact": summary.skipped_do_not_contact,
                "considered": summary.considered(),
            }),
        ),
        Err(error) => {
            tracing::warn!(%error, "Latarnik SubmitHub beacon import failed");
            BeaconSignalError::Unavailable.response(request_id_value)
        }
    }
}

async fn request_discovery(
    state: &crate::AppState,
    headers: &HeaderMap,
    payload: AdminNetworkActionRequest,
    idempotency_key: String,
) -> Response {
    let request_id_value = request_id(headers);
    let country_code = payload
        .country_code
        .unwrap_or_else(|| "PL".to_owned())
        .trim()
        .to_uppercase();
    let target_count = payload.target_count.unwrap_or(100);
    if country_code.len() != 2
        || !country_code.bytes().all(|byte| byte.is_ascii_uppercase())
        || !(1..=MAX_DISCOVERY_TARGET).contains(&target_count)
    {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    };
    if let Ok(Some((action, target_id))) = sqlx::query_as::<_, (String, Uuid)>(
        "SELECT action,target_id FROM operator_actions WHERE workspace_id=$1 AND idempotency_key=$2",
    )
    .bind(workspace_id)
    .bind(&idempotency_key)
    .fetch_optional(&mut *tx)
    .await
    {
        if action != "request_beacon_network_discovery" {
            return BeaconSignalError::Conflict.response(request_id_value);
        }
        if tx.commit().await.is_err() {
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
        return private_json(StatusCode::OK, json!({"runId": target_id, "replayed": true}));
    }
    match executor_capability_available_tx(&mut tx, workspace_id, "beacon.network.discovery").await
    {
        Ok(true) => {}
        Ok(false) => return BeaconSignalError::Unavailable.response(request_id_value),
        Err(error) => {
            tracing::warn!(%error, "Latarnik discovery executor capability lookup failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    }
    let run_id = Uuid::now_v7();
    if let Err(error) = sqlx::query(
        "INSERT INTO viryaos_beacon_network_discovery_runs (id,workspace_id,country_code,target_count) VALUES ($1,$2,$3,$4)",
    )
    .bind(run_id)
    .bind(workspace_id)
    .bind(&country_code)
    .bind(target_count)
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, "Latarnik discovery run insert failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    if let Err(error) = sqlx::query(
        r#"
        INSERT INTO outbox_events (workspace_id,event_type,event_version,payload,request_id,max_attempts)
        VALUES ($1,'crowdrelay.beacon.network_discovery_requested',1,$2,$3,12)
        "#,
    )
    .bind(workspace_id)
    .bind(json!({"run_id": run_id, "country_code": country_code, "target_count": target_count}))
    .bind(format!("beacon-network-discovery:{run_id}"))
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, "Latarnik discovery outbox insert failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    match record_operator_action(
        &mut tx,
        workspace_id,
        OperatorActionRecord {
            action: "request_beacon_network_discovery",
            target_type: "beacon_network_discovery_run",
            target_id: run_id,
            idempotency_key: &idempotency_key,
            request_id: request_id_value.as_deref(),
            details: json!({"country_code": country_code, "target_count": target_count}),
        },
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => return BeaconSignalError::Conflict.response(request_id_value),
        Err(error) => {
            tracing::warn!(%error, "Latarnik discovery audit failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    }
    if tx.commit().await.is_err() {
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    private_json(
        StatusCode::ACCEPTED,
        json!({"runId": run_id, "status": "requested"}),
    )
}

async fn approve_candidate(
    state: &crate::AppState,
    headers: &HeaderMap,
    payload: AdminNetworkActionRequest,
    idempotency_key: String,
) -> Response {
    let request_id_value = request_id(headers);
    let (Some(beacon_id), Some(evidence_url)) = (payload.beacon_id, payload.consent_evidence_url)
    else {
        return BeaconSignalError::BadRequest.response(request_id_value);
    };
    if payload.source_verified != Some(true)
        || payload.marketing_email_consent_confirmed != Some(true)
        || !valid_https_url(&evidence_url)
    {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    };
    if let Ok(Some((action, target_id))) = sqlx::query_as::<_, (String, Uuid)>(
        "SELECT action,target_id FROM operator_actions WHERE workspace_id=$1 AND idempotency_key=$2",
    )
    .bind(workspace_id)
    .bind(&idempotency_key)
    .fetch_optional(&mut *tx)
    .await
    {
        if action != "approve_beacon_network_candidate" || target_id != beacon_id {
            return BeaconSignalError::Conflict.response(request_id_value);
        }
        if tx.commit().await.is_err() {
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
        return private_json(StatusCode::OK, json!({"beaconId": beacon_id, "replayed": true}));
    }
    let candidate = match sqlx::query_as::<_, (bool, Option<String>, Value)>(
        r#"
        SELECT do_not_contact,source_url,metadata
        FROM viryaos_beacons
        WHERE workspace_id=$1 AND id=$2 AND active AND metadata ? 'network_discovery_run_id'
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .fetch_optional(&mut *tx)
    .await
    {
        Ok(Some(value)) => value,
        Ok(None) => return BeaconSignalError::NotFound.response(request_id_value),
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    };
    if candidate.0
        || candidate
            .1
            .as_deref()
            .is_none_or(|value| !valid_https_url(value))
    {
        return BeaconSignalError::Conflict.response(request_id_value);
    }
    let review = json!({
        "source_verified": true,
        "marketing_email_consent_confirmed": true,
        "consent_evidence_url": evidence_url,
        "reviewed_at": OffsetDateTime::now_utc(),
    });
    if let Err(error) = sqlx::query(
        r#"
        UPDATE viryaos_beacons
        SET verified=true,accepts_outreach=true,
            metadata=metadata || jsonb_build_object('network_review',$3::jsonb),
            version=version+1
        WHERE workspace_id=$1 AND id=$2 AND NOT do_not_contact
        "#,
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .bind(review.clone())
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, "Latarnik network approval failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    match record_operator_action(
        &mut tx,
        workspace_id,
        OperatorActionRecord {
            action: "approve_beacon_network_candidate",
            target_type: "beacon",
            target_id: beacon_id,
            idempotency_key: &idempotency_key,
            request_id: request_id_value.as_deref(),
            details: review,
        },
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => return BeaconSignalError::Conflict.response(request_id_value),
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    }
    if tx.commit().await.is_err() {
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    private_json(
        StatusCode::OK,
        json!({"beaconId": beacon_id, "verified": true, "acceptsOutreach": true}),
    )
}

async fn preview_invites(
    state: &crate::AppState,
    headers: &HeaderMap,
    payload: AdminNetworkActionRequest,
) -> Response {
    let request_id_value = request_id(headers);
    let Some(mut beacon_ids) = payload.beacon_ids else {
        return BeaconSignalError::BadRequest.response(request_id_value);
    };
    beacon_ids.sort_unstable();
    let before = beacon_ids.len();
    beacon_ids.dedup();
    let ttl_days = payload.ttl_days.unwrap_or(DEFAULT_INVITE_TTL_DAYS as i32);
    let radius_km = payload.radius_km.unwrap_or(DEFAULT_RADIUS_KM);
    let locale = payload.locale.unwrap_or_else(default_locale);
    if beacon_ids.is_empty()
        || beacon_ids.len() > MAX_INVITE_BATCH
        || beacon_ids.len() != before
        || !(1..=MAX_INVITE_TTL_DAYS as i32).contains(&ttl_days)
        || !valid_radius(radius_km)
        || !valid_locale(&locale)
    {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let rows = match sqlx::query_as::<_, (String, i64)>(
        r#"
        SELECT beacon.beacon_kind,count(*)::bigint
        FROM viryaos_beacons beacon
        LEFT JOIN viryaos_beacon_signal_profiles profile
          ON profile.workspace_id=beacon.workspace_id AND profile.beacon_id=beacon.id
        WHERE beacon.workspace_id=$1 AND beacon.id=ANY($2)
          AND beacon.active AND beacon.verified AND beacon.accepts_outreach
          AND NOT beacon.do_not_contact AND beacon.contact_email IS NOT NULL
          AND COALESCE(profile.status,'') <> 'active'
          -- A never-invited beacon has no profile row at all. Three-valued logic
          -- turns the bare NOT(...) into NULL there and silently drops exactly the
          -- first-wave candidates this flow exists to reach, so fold NULL to false.
          AND NOT COALESCE(profile.status='invited' AND profile.invite_expires_at > now(), false)
          AND (
            NOT (beacon.metadata ? 'network_discovery_run_id')
            OR (
              beacon.metadata #>> '{network_review,source_verified}' = 'true'
              AND beacon.metadata #>> '{network_review,marketing_email_consent_confirmed}' = 'true'
              AND COALESCE(beacon.metadata #>> '{network_review,consent_evidence_url}','') LIKE 'https://%'
            )
          )
        GROUP BY beacon.beacon_kind
        ORDER BY beacon.beacon_kind
        "#,
    )
    .bind(workspace_id)
    .bind(&beacon_ids)
    .fetch_all(state.ticketing.pool())
    .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Latarnik invite preview eligibility lookup failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let eligible_count: i64 = rows.iter().map(|(_, count)| *count).sum();
    if eligible_count != beacon_ids.len() as i64 {
        return BeaconSignalError::Conflict.response(request_id_value);
    }
    let by_kind = rows
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>();
    let brand = match crowdrelay_infra::tenant_settings::TenantSettingsRepository::new(
        state.database.clone(),
    )
    .brand_settings(state.ticketing.workspace_id().into_uuid())
    .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "Tenant brand settings lookup failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    let placeholder_url = brand.invite_url(&locale, "<jednorazowy-link>");
    let delivery = invite_delivery_copy(&locale, "{{displayName}}", &placeholder_url);
    private_json(
        StatusCode::OK,
        json!({
            "version": 1,
            "beaconCount": beacon_ids.len(),
            "ttlDays": ttl_days,
            "radiusKm": radius_km,
            "locale": locale,
            "byKind": by_kind,
            "delivery": delivery,
            "tokensMinted": false,
        }),
    )
}

async fn queue_invites(
    state: &crate::AppState,
    headers: &HeaderMap,
    payload: AdminNetworkActionRequest,
    idempotency_key: String,
) -> Response {
    let request_id_value = request_id(headers);
    let Some(mut beacon_ids) = payload.beacon_ids else {
        return BeaconSignalError::BadRequest.response(request_id_value);
    };
    beacon_ids.sort_unstable();
    let before = beacon_ids.len();
    beacon_ids.dedup();
    let ttl_days = payload.ttl_days.unwrap_or(DEFAULT_INVITE_TTL_DAYS as i32);
    let radius_km = payload.radius_km.unwrap_or(DEFAULT_RADIUS_KM);
    let locale = payload.locale.unwrap_or_else(default_locale);
    if beacon_ids.is_empty()
        || beacon_ids.len() > MAX_INVITE_BATCH
        || beacon_ids.len() != before
        || !(1..=MAX_INVITE_TTL_DAYS as i32).contains(&ttl_days)
        || !valid_radius(radius_km)
        || !valid_locale(&locale)
    {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    };
    if let Ok(Some((action, target_id))) = sqlx::query_as::<_, (String, Uuid)>(
        "SELECT action,target_id FROM operator_actions WHERE workspace_id=$1 AND idempotency_key=$2",
    )
    .bind(workspace_id)
    .bind(&idempotency_key)
    .fetch_optional(&mut *tx)
    .await
    {
        if action != "queue_beacon_invite_delivery" {
            return BeaconSignalError::Conflict.response(request_id_value);
        }
        if tx.commit().await.is_err() {
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
        return private_json(StatusCode::OK, json!({"jobId": target_id, "replayed": true}));
    }
    match executor_capability_available_tx(&mut tx, workspace_id, "beacon.network.invite").await {
        Ok(true) => {}
        Ok(false) => return BeaconSignalError::Unavailable.response(request_id_value),
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    }
    let eligible_count = match sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM viryaos_beacons beacon
        LEFT JOIN viryaos_beacon_signal_profiles profile
          ON profile.workspace_id=beacon.workspace_id AND profile.beacon_id=beacon.id
        WHERE beacon.workspace_id=$1 AND beacon.id=ANY($2)
          AND beacon.active AND beacon.verified AND beacon.accepts_outreach
          AND NOT beacon.do_not_contact AND beacon.contact_email IS NOT NULL
          AND COALESCE(profile.status,'') <> 'active'
          -- A never-invited beacon has no profile row at all. Three-valued logic
          -- turns the bare NOT(...) into NULL there and silently drops exactly the
          -- first-wave candidates this flow exists to reach, so fold NULL to false.
          AND NOT COALESCE(profile.status='invited' AND profile.invite_expires_at > now(), false)
          AND (
            NOT (beacon.metadata ? 'network_discovery_run_id')
            OR (
              beacon.metadata #>> '{network_review,source_verified}' = 'true'
              AND beacon.metadata #>> '{network_review,marketing_email_consent_confirmed}' = 'true'
              AND COALESCE(beacon.metadata #>> '{network_review,consent_evidence_url}','') LIKE 'https://%'
            )
          )
        "#,
    )
    .bind(workspace_id)
    .bind(&beacon_ids)
    .fetch_one(&mut *tx)
    .await
    {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    };
    if eligible_count != beacon_ids.len() as i64 {
        return BeaconSignalError::Conflict.response(request_id_value);
    }
    let job_id = Uuid::now_v7();
    if let Err(error) = sqlx::query(
        r#"
        INSERT INTO viryaos_beacon_invite_delivery_jobs
          (id,workspace_id,beacon_ids,ttl_days,radius_km,locale)
        VALUES ($1,$2,$3,$4,$5,$6)
        "#,
    )
    .bind(job_id)
    .bind(workspace_id)
    .bind(&beacon_ids)
    .bind(ttl_days)
    .bind(radius_km)
    .bind(&locale)
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, "Latarnik invite delivery job insert failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    if let Err(error) = sqlx::query(
        r#"
        INSERT INTO outbox_events (workspace_id,event_type,event_version,payload,request_id,max_attempts)
        VALUES ($1,'crowdrelay.beacon.invite_delivery_requested',1,$2,$3,12)
        "#,
    )
    .bind(workspace_id)
    .bind(json!({"job_id": job_id}))
    .bind(format!("beacon-invite-delivery:{job_id}"))
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, "Latarnik invite delivery outbox insert failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    match record_operator_action(
        &mut tx,
        workspace_id,
        OperatorActionRecord {
            action: "queue_beacon_invite_delivery",
            target_type: "beacon_invite_delivery_job",
            target_id: job_id,
            idempotency_key: &idempotency_key,
            request_id: request_id_value.as_deref(),
            details: json!({"beacon_count": beacon_ids.len(), "ttl_days": ttl_days, "radius_km": radius_km, "locale": locale}),
        },
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => return BeaconSignalError::Conflict.response(request_id_value),
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    }
    if tx.commit().await.is_err() {
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    private_json(
        StatusCode::ACCEPTED,
        json!({"jobId": job_id, "status": "queued", "beaconCount": beacon_ids.len()}),
    )
}
