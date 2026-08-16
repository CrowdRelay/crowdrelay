use super::*;

pub async fn internal_ingest_discovered_beacons(
    State(state): State<crate::AppState>,
    Path(run_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<IngestDiscoveryRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    if payload.candidates.is_empty() || payload.candidates.len() > MAX_DISCOVERY_BATCH {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    for candidate in &payload.candidates {
        let invalid = !valid_beacon_kind(candidate.beacon_kind.trim())
            || clean_text(&candidate.display_name, 240).is_none()
            || !valid_https_url(candidate.source_url.trim())
            || candidate
                .contact_email
                .as_deref()
                .is_some_and(|value| !valid_email(value))
            || candidate
                .destination_url
                .as_deref()
                .is_some_and(|value| !valid_https_url(value))
            || candidate
                .source_note
                .as_deref()
                .is_some_and(|value| value.trim().is_empty() || value.len() > MAX_EVIDENCE_LEN)
            || candidate
                .relevance_basis_points
                .is_some_and(|value| !(0..=10_000).contains(&value))
            || candidate
                .confidence_basis_points
                .is_some_and(|value| !(0..=10_000).contains(&value));
        if invalid {
            return BeaconSignalError::BadRequest.response(request_id_value);
        }
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    };
    let run = match sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT status,country_code
        FROM viryaos_beacon_network_discovery_runs
        WHERE workspace_id=$1 AND id=$2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(run_id)
    .fetch_optional(&mut *tx)
    .await
    {
        Ok(Some(value)) => value,
        Ok(None) => return BeaconSignalError::NotFound.response(request_id_value),
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    };
    if !matches!(run.0.as_str(), "requested" | "running") {
        return BeaconSignalError::Conflict.response(request_id_value);
    }
    let mut inserted = 0usize;
    let mut existing = 0usize;
    for candidate in payload.candidates {
        let display_name = candidate.display_name.trim().to_owned();
        let beacon_kind = candidate.beacon_kind.trim().to_owned();
        let contact_email = candidate
            .contact_email
            .map(|value| value.trim().to_lowercase());
        let destination_url = candidate
            .destination_url
            .map(|value| value.trim().to_owned());
        let source_url = candidate.source_url.trim().to_owned();
        let relevance = candidate.relevance_basis_points.unwrap_or(5000);
        let confidence = candidate.confidence_basis_points.unwrap_or(5000);
        let metadata = json!({
            "network_discovery_run_id": run_id,
            "network_discovery": {
                "country_code": run.1,
                "source_url": source_url,
                "source_note": candidate.source_note,
                "human_review_required": true,
                "marketing_email_consent_confirmed": false,
                "discovered_at": OffsetDateTime::now_utc(),
            }
        });
        let found = match sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT id
            FROM viryaos_beacons
            WHERE workspace_id=$1 AND beacon_kind=$2 AND city_id IS NOT DISTINCT FROM $3
              AND (
                ($4::text IS NOT NULL AND contact_email=$4)
                OR ($4::text IS NULL AND $5::text IS NOT NULL AND contact_email IS NULL AND destination_url=$5)
              )
            ORDER BY id
            LIMIT 1
            FOR UPDATE
            "#,
        )
        .bind(workspace_id)
        .bind(&beacon_kind)
        .bind(candidate.city_id)
        .bind(contact_email.as_deref())
        .bind(destination_url.as_deref())
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(value) => value,
            Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
        };
        if let Some(beacon_id) = found {
            if let Err(error) = sqlx::query(
                r#"
                UPDATE viryaos_beacons
                SET source_url=COALESCE(source_url,$3),
                    metadata=metadata || $4::jsonb,
                    relevance_basis_points=GREATEST(relevance_basis_points,$5),
                    confidence_basis_points=GREATEST(confidence_basis_points,$6),
                    version=version+1
                WHERE workspace_id=$1 AND id=$2
                "#,
            )
            .bind(workspace_id)
            .bind(beacon_id)
            .bind(&source_url)
            .bind(metadata)
            .bind(relevance)
            .bind(confidence)
            .execute(&mut *tx)
            .await
            {
                tracing::warn!(%error, %beacon_id, "Latarnik discovery candidate refresh failed");
                return BeaconSignalError::Unavailable.response(request_id_value);
            }
            existing += 1;
            continue;
        }
        if contact_email.is_none() && destination_url.is_none() {
            continue;
        }
        let result = sqlx::query(
            r#"
            INSERT INTO viryaos_beacons (
                id,workspace_id,city_id,beacon_kind,display_name,contact_email,destination_url,
                source_url,active,verified,accepts_outreach,do_not_contact,
                relationship_score,relevance_basis_points,confidence_basis_points,metadata
            ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,true,false,false,false,50,$9,$10,$11)
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id)
        .bind(candidate.city_id)
        .bind(&beacon_kind)
        .bind(&display_name)
        .bind(contact_email)
        .bind(destination_url)
        .bind(&source_url)
        .bind(relevance)
        .bind(confidence)
        .bind(metadata)
        .execute(&mut *tx)
        .await;
        match result {
            Ok(_) => inserted += 1,
            Err(error) => {
                tracing::warn!(%error, "Latarnik discovery candidate insert failed");
                return BeaconSignalError::Unavailable.response(request_id_value);
            }
        }
    }
    if let Err(error) = sqlx::query(
        r#"
        UPDATE viryaos_beacon_network_discovery_runs
        SET status='running',started_at=COALESCE(started_at,now()),
            discovered_count=GREATEST(discovered_count,$3)
        WHERE workspace_id=$1 AND id=$2
        "#,
    )
    .bind(workspace_id)
    .bind(run_id)
    .bind((inserted + existing) as i32)
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, "Latarnik discovery run progress update failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    if tx.commit().await.is_err() {
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    private_json(
        StatusCode::OK,
        json!({"runId": run_id, "inserted": inserted, "existing": existing, "reviewRequired": true}),
    )
}

pub async fn internal_report_discovery_run(
    State(state): State<crate::AppState>,
    Path(run_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<DiscoveryReportRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    if !matches!(payload.status.as_str(), "ready" | "failed")
        || payload.discovered_count < 0
        || payload
            .report_filename
            .as_deref()
            .is_some_and(|value| value.trim().is_empty() || value.len() > MAX_REPORT_FILENAME_LEN)
        || payload
            .report_sha256
            .as_deref()
            .is_some_and(|value| !valid_sha256(value))
        || payload
            .failure_kind
            .as_deref()
            .is_some_and(|value| value.trim().is_empty() || value.len() > MAX_FAILURE_KIND_LEN)
        || (payload.status == "ready" && payload.failure_kind.is_some())
        || (payload.status == "failed" && payload.failure_kind.is_none())
    {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let result = sqlx::query(
        r#"
        UPDATE viryaos_beacon_network_discovery_runs
        SET status=$3,discovered_count=$4,report_filename=$5,report_sha256=$6,
            failure_kind=$7,completed_at=COALESCE(completed_at,now()),
            started_at=COALESCE(started_at,requested_at)
        WHERE workspace_id=$1 AND id=$2 AND status IN ('requested','running')
        "#,
    )
    .bind(workspace_id)
    .bind(run_id)
    .bind(&payload.status)
    .bind(payload.discovered_count)
    .bind(payload.report_filename.as_deref())
    .bind(payload.report_sha256.as_deref())
    .bind(payload.failure_kind.as_deref())
    .execute(state.ticketing.pool())
    .await;
    match result {
        Ok(value) if value.rows_affected() == 1 => private_json(
            StatusCode::OK,
            json!({"runId": run_id, "status": payload.status, "discoveredCount": payload.discovered_count}),
        ),
        Ok(_) => BeaconSignalError::Conflict.response(request_id_value),
        Err(error) => {
            tracing::warn!(%error, "Latarnik discovery report failed");
            BeaconSignalError::Unavailable.response(request_id_value)
        }
    }
}

pub async fn internal_claim_invite_delivery_job(
    State(state): State<crate::AppState>,
    Path(job_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<InviteJobClaimRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    let Some(worker_id) = clean_text(&payload.worker_id, 120) else {
        return BeaconSignalError::BadRequest.response(request_id_value);
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    };
    let job = match sqlx::query_as::<_, (Vec<Uuid>, i32, i32, String, String)>(
        r#"
        SELECT beacon_ids,ttl_days,radius_km,locale,status
        FROM viryaos_beacon_invite_delivery_jobs
        WHERE workspace_id=$1 AND id=$2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(job_id)
    .fetch_optional(&mut *tx)
    .await
    {
        Ok(Some(value)) => value,
        Ok(None) => return BeaconSignalError::NotFound.response(request_id_value),
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    };
    if job.4 != "queued" {
        return BeaconSignalError::Conflict.response(request_id_value);
    }
    let Some(claim_token) = random_token::<24>() else {
        return BeaconSignalError::Unavailable.response(request_id_value);
    };
    let batch = match lifecycle::mint_invite_batch_tx(
        &mut tx,
        workspace_id,
        &job.0,
        i64::from(job.1),
        job.2,
        &job.3,
    )
    .await
    {
        Ok(value) => value,
        Err(error) => return error.response(request_id_value),
    };
    if batch.created != job.0.len() || batch.skipped != 0 {
        tracing::info!(job_id=%job_id, created=batch.created, requested=job.0.len(), "Latarnik invite claim lost eligibility race");
        return BeaconSignalError::Conflict.response(request_id_value);
    }
    if let Err(error) = sqlx::query(
        r#"
        UPDATE viryaos_beacon_invite_delivery_jobs
        SET status='claimed',claim_token_hash=$3,claimed_by=$4,claimed_at=now()
        WHERE workspace_id=$1 AND id=$2 AND status='queued'
        "#,
    )
    .bind(workspace_id)
    .bind(job_id)
    .bind(token_hash(&claim_token))
    .bind(&worker_id)
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, "Latarnik invite delivery claim persistence failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    if tx.commit().await.is_err() {
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    private_json(
        StatusCode::OK,
        InviteJobClaimResponse {
            version: 1,
            job_id,
            claim_token,
            batch,
        },
    )
}

pub async fn internal_report_invite_delivery_job(
    State(state): State<crate::AppState>,
    Path(job_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<InviteJobReportRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::BadRequest.response(request_id_value),
    };
    if !matches!(
        payload.status.as_str(),
        "completed" | "failed" | "ambiguous"
    ) || !payload.provider_summary.is_object()
        || !valid_invite_token(&payload.claim_token)
    {
        return BeaconSignalError::BadRequest.response(request_id_value);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = match state.ticketing.pool().begin().await {
        Ok(value) => value,
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    };
    let current = match sqlx::query_as::<_, (String, Option<Vec<u8>>)>(
        r#"
        SELECT status,claim_token_hash
        FROM viryaos_beacon_invite_delivery_jobs
        WHERE workspace_id=$1 AND id=$2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(job_id)
    .fetch_optional(&mut *tx)
    .await
    {
        Ok(Some(value)) => value,
        Ok(None) => return BeaconSignalError::NotFound.response(request_id_value),
        Err(_) => return BeaconSignalError::Unavailable.response(request_id_value),
    };
    let supplied_hash = token_hash(&payload.claim_token);
    if current.1.as_deref() != Some(supplied_hash.as_slice()) {
        return BeaconSignalError::Unauthorized.response(request_id_value);
    }
    if matches!(current.0.as_str(), "completed" | "failed" | "ambiguous") {
        if current.0 != payload.status {
            return BeaconSignalError::Conflict.response(request_id_value);
        }
        if tx.commit().await.is_err() {
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
        return private_json(
            StatusCode::OK,
            json!({"jobId": job_id, "status": current.0, "replayed": true}),
        );
    }
    if current.0 != "claimed" {
        return BeaconSignalError::Conflict.response(request_id_value);
    }
    if let Err(error) = sqlx::query(
        r#"
        UPDATE viryaos_beacon_invite_delivery_jobs
        SET status=$3,provider_summary=$4,reported_at=now()
        WHERE workspace_id=$1 AND id=$2 AND status='claimed'
        "#,
    )
    .bind(workspace_id)
    .bind(job_id)
    .bind(&payload.status)
    .bind(payload.provider_summary)
    .execute(&mut *tx)
    .await
    {
        tracing::warn!(%error, "Latarnik invite delivery report persistence failed");
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    if tx.commit().await.is_err() {
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    private_json(
        StatusCode::OK,
        json!({"jobId": job_id, "status": payload.status}),
    )
}
