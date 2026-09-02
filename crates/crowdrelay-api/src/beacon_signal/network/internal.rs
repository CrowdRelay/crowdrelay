use std::collections::BTreeMap;

use crowdrelay_domain::BeaconContactIdentity;

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
    let mut observations: BTreeMap<Uuid, (String, Option<String>, i32, i32)> = BTreeMap::new();
    for candidate in payload.candidates {
        let display_name = candidate.display_name.trim().to_owned();
        let beacon_kind = candidate.beacon_kind.trim().to_owned();
        let contact_email = candidate
            .contact_email
            .map(|value| value.trim().to_lowercase());
        let destination_url = candidate
            .destination_url
            .map(|value| value.trim().to_owned());
        if contact_email.is_none() && destination_url.is_none() {
            continue;
        }
        let source_url = candidate.source_url.trim().to_owned();
        let source_note = candidate.source_note.map(|value| value.trim().to_owned());
        let relevance = candidate.relevance_basis_points.unwrap_or(5000);
        let confidence = candidate.confidence_basis_points.unwrap_or(5000);

        // A partial unique index protects the database, while this deterministic
        // transaction lock makes two concurrent discovery runs idempotent rather
        // than surfacing a uniqueness race as a 503.
        let Some(contact_identity) = BeaconContactIdentity::from_normalized(
            contact_email.as_deref(),
            destination_url.as_deref(),
        ) else {
            continue;
        };
        let identity = format!(
            "beacon-discovery:{workspace_id}:{beacon_kind}:{}:{}:{}",
            candidate
                .city_id
                .map_or_else(|| "none".to_owned(), |id| id.to_string()),
            contact_identity.namespace(),
            contact_identity.value(),
        );
        if let Err(error) = sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(&identity)
            .execute(&mut *tx)
            .await
        {
            tracing::warn!(%error, "Latarnik discovery identity lock failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }

        let metadata = json!({
            "network_discovery_run_id": run_id,
            "network_discovery": {
                "country_code": &run.1,
                "source_url": &source_url,
                "source_note": source_note.as_deref(),
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
            Err(error) => {
                tracing::warn!(%error, "Latarnik discovery candidate lookup failed");
                return BeaconSignalError::Unavailable.response(request_id_value);
            }
        };

        let beacon_id = if let Some(beacon_id) = found {
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
            .bind(&metadata)
            .bind(relevance)
            .bind(confidence)
            .execute(&mut *tx)
            .await
            {
                tracing::warn!(%error, %beacon_id, "Latarnik discovery candidate refresh failed");
                return BeaconSignalError::Unavailable.response(request_id_value);
            }
            existing += 1;
            beacon_id
        } else {
            let beacon_id = Uuid::now_v7();
            if let Err(error) = sqlx::query(
                r#"
                INSERT INTO viryaos_beacons (
                    id,workspace_id,city_id,beacon_kind,display_name,contact_email,destination_url,
                    source_url,active,verified,accepts_outreach,do_not_contact,
                    relationship_score,relevance_basis_points,confidence_basis_points,metadata
                ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,true,false,false,false,50,$9,$10,$11)
                "#,
            )
            .bind(beacon_id)
            .bind(workspace_id)
            .bind(candidate.city_id)
            .bind(&beacon_kind)
            .bind(&display_name)
            .bind(contact_email.as_deref())
            .bind(destination_url.as_deref())
            .bind(&source_url)
            .bind(relevance)
            .bind(confidence)
            .bind(&metadata)
            .execute(&mut *tx)
            .await
            {
                tracing::warn!(%error, "Latarnik discovery candidate insert failed");
                return BeaconSignalError::Unavailable.response(request_id_value);
            }
            inserted += 1;
            beacon_id
        };

        observations
            .entry(beacon_id)
            .and_modify(|current| {
                current.0.clone_from(&source_url);
                current.1.clone_from(&source_note);
                current.2 = current.2.max(relevance);
                current.3 = current.3.max(confidence);
            })
            .or_insert((source_url, source_note, relevance, confidence));
    }

    if !observations.is_empty() {
        let beacon_ids: Vec<Uuid> = observations.keys().copied().collect();
        let source_urls: Vec<String> = observations
            .values()
            .map(|observation| observation.0.clone())
            .collect();
        let source_notes: Vec<String> = observations
            .values()
            .map(|observation| observation.1.clone().unwrap_or_default())
            .collect();
        let relevances: Vec<i32> = observations
            .values()
            .map(|observation| observation.2)
            .collect();
        let confidences: Vec<i32> = observations
            .values()
            .map(|observation| observation.3)
            .collect();
        if let Err(error) = sqlx::query(
            r#"
            INSERT INTO viryaos_beacon_network_discovery_observations
              (workspace_id,run_id,beacon_id,source_url,source_note,relevance_basis_points,confidence_basis_points)
            SELECT $1,$2,candidate.beacon_id,candidate.source_url,
                   NULLIF(candidate.source_note,''),candidate.relevance,candidate.confidence
            FROM unnest($3::uuid[],$4::text[],$5::text[],$6::int4[],$7::int4[])
              AS candidate(beacon_id,source_url,source_note,relevance,confidence)
            ON CONFLICT (workspace_id,run_id,beacon_id) DO UPDATE SET
              source_url=EXCLUDED.source_url,
              source_note=EXCLUDED.source_note,
              relevance_basis_points=GREATEST(viryaos_beacon_network_discovery_observations.relevance_basis_points,EXCLUDED.relevance_basis_points),
              confidence_basis_points=GREATEST(viryaos_beacon_network_discovery_observations.confidence_basis_points,EXCLUDED.confidence_basis_points)
            "#,
        )
        .bind(workspace_id)
        .bind(run_id)
        .bind(&beacon_ids)
        .bind(&source_urls)
        .bind(&source_notes)
        .bind(&relevances)
        .bind(&confidences)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(%error, "Latarnik discovery observation bulk persistence failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    }

    let discovered_count = match sqlx::query_scalar::<_, i32>(
        r#"
        UPDATE viryaos_beacon_network_discovery_runs AS run
        SET status='running',started_at=COALESCE(started_at,now()),
            discovered_count=(
              SELECT count(*)::int
              FROM viryaos_beacon_network_discovery_observations observation
              WHERE observation.workspace_id=run.workspace_id AND observation.run_id=run.id
            )
        WHERE run.workspace_id=$1 AND run.id=$2
        RETURNING discovered_count
        "#,
    )
    .bind(workspace_id)
    .bind(run_id)
    .fetch_optional(&mut *tx)
    .await
    {
        Ok(Some(value)) => value,
        Ok(None) => return BeaconSignalError::NotFound.response(request_id_value),
        Err(error) => {
            tracing::warn!(%error, "Latarnik discovery canonical progress update failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    if tx.commit().await.is_err() {
        return BeaconSignalError::Unavailable.response(request_id_value);
    }
    private_json(
        StatusCode::OK,
        json!({
            "runId": run_id,
            "inserted": inserted,
            "existing": existing,
            "discoveredCount": discovered_count,
            "reviewRequired": true
        }),
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
    // `discoveredCount` is retained in the executor contract for diagnostics,
    // but PostgreSQL owns the canonical count. Never let a stale/smaller final
    // report overwrite the cumulative observations recorded by earlier batches.
    let result = sqlx::query_scalar::<_, i32>(
        r#"
        UPDATE viryaos_beacon_network_discovery_runs AS run
        SET status=$3,
            discovered_count=(
              SELECT count(*)::integer
              FROM viryaos_beacon_network_discovery_observations AS observation
              WHERE observation.workspace_id=run.workspace_id AND observation.run_id=run.id
            ),
            report_filename=$4,report_sha256=$5,
            failure_kind=$6,completed_at=COALESCE(completed_at,now()),
            started_at=COALESCE(started_at,requested_at)
        WHERE workspace_id=$1 AND id=$2 AND status IN ('requested','running')
        RETURNING discovered_count
        "#,
    )
    .bind(workspace_id)
    .bind(run_id)
    .bind(&payload.status)
    .bind(payload.report_filename.as_deref())
    .bind(payload.report_sha256.as_deref())
    .bind(payload.failure_kind.as_deref())
    .fetch_optional(state.ticketing.pool())
    .await;
    match result {
        Ok(Some(discovered_count)) => {
            if discovered_count != payload.discovered_count {
                tracing::info!(
                    run_id=%run_id,
                    executor_count=payload.discovered_count,
                    canonical_count=discovered_count,
                    "Latarnik discovery final count reconciled to canonical observations"
                );
            }
            private_json(
                StatusCode::OK,
                json!({"runId": run_id, "status": payload.status, "discoveredCount": discovered_count}),
            )
        }
        Ok(None) => BeaconSignalError::Conflict.response(request_id_value),
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
    let batch = match lifecycle::mint_invite_batch_tx(
        &mut tx,
        &brand,
        workspace_id,
        &job.0,
        i64::from(job.1),
        job.2,
        &job.3,
        Some(job_id),
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
    // The `status='queued'` predicate is the whole race guard, so its result
    // has to be read.
    //
    // This only checked for an error. Two workers claiming the same job both
    // got HTTP 200 and a claim token: the winner's UPDATE matched, the
    // loser's matched nothing, and nobody noticed. The loser then delivered
    // the same invites — the beacons receive them twice — and only discovered
    // it had never held the claim when its report failed token validation,
    // long after the mail had gone.
    let claimed = match sqlx::query(
        r#"
        UPDATE viryaos_beacon_invite_delivery_jobs
        SET status='claimed',claim_token_hash=$3,claimed_by=$4,claimed_at=now(),
            claim_expires_at=now()+interval '60 minutes'
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
        Ok(result) => result.rows_affected(),
        Err(error) => {
            tracing::warn!(%error, "Latarnik invite delivery claim persistence failed");
            return BeaconSignalError::Unavailable.response(request_id_value);
        }
    };
    if claimed == 0 {
        // Somebody else holds it, or it is no longer queued. Conflict is the
        // honest answer and it stops this worker before it sends anything.
        tracing::info!(
            %job_id,
            worker_id = %worker_id,
            "Latarnik invite delivery claim lost the race; another worker holds this job"
        );
        return BeaconSignalError::Conflict.response(request_id_value);
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
    let current = match sqlx::query_as::<_, (String, Option<Vec<u8>>, serde_json::Value)>(
        r#"
        SELECT status,claim_token_hash,provider_summary
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
        if current.0 != payload.status || current.2 != payload.provider_summary {
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
