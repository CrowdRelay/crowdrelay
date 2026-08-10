async fn claim_batches(
    state: &crate::AppState,
    worker_id: &str,
    lease_seconds: i64,
    limit: i64,
) -> Result<ClaimResponse, ProofError> {
    if !matches!(
        ecosystem::feature_enabled(state, "external_proof_anchoring_enabled").await,
        Ok(true)
    ) {
        return Ok(ClaimResponse {
            batches: Vec::new(),
        });
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(ProofError::sqlx)?;
    configure_transaction(&mut tx, state).await?;
    sqlx::query(
        r#"
        WITH recovered AS (
            UPDATE external_proof_batches
            SET status = CASE WHEN attempts >= max_attempts THEN 'dead' ELSE 'failed' END,
                locked_at = NULL, lock_owner = NULL, lease_expires_at = NULL,
                available_at = CASE WHEN attempts >= max_attempts THEN available_at ELSE now() END,
                last_error_kind = COALESCE(last_error_kind, 'lease_expired')
            WHERE workspace_id = $1 AND status = 'processing'
              AND lease_expires_at <= now()
            RETURNING id, proof_kind, root_sha256, attempts, status, last_error_kind
        )
        INSERT INTO outbox_events (
            workspace_id, event_type, event_version, payload, request_id
        )
        SELECT $1, 'blockchain.proof.dead', 1,
               jsonb_build_object(
                   'batch_id', id,
                   'proof_kind', proof_kind,
                   'root_sha256', encode(root_sha256, 'hex'),
                   'attempts', attempts,
                   'error_kind', last_error_kind
               ),
               id::text
        FROM recovered
        WHERE status = 'dead'
        "#,
    )
    .bind(workspace_id)
    .execute(&mut *tx)
    .await
    .map_err(ProofError::sqlx)?;

    #[derive(FromRow)]
    struct Claimed {
        id: Uuid,
        proof_kind: String,
        schema_version: i32,
        root_sha256: Vec<u8>,
        leaf_count: i32,
        tree_algorithm: String,
        attempts: i32,
        lease_expires_at: OffsetDateTime,
    }
    let row = sqlx::query_as::<_, Claimed>(
        r#"
        WITH candidate AS (
            SELECT id
            FROM external_proof_batches
            WHERE workspace_id = $1
              AND status IN ('queued', 'failed')
              AND available_at <= now()
              AND attempts < max_attempts
            ORDER BY available_at, created_at, id
            FOR UPDATE SKIP LOCKED
            LIMIT $4
        )
        UPDATE external_proof_batches AS batch
        SET status = 'processing', attempts = attempts + 1,
            locked_at = now(), lock_owner = $2,
            lease_expires_at = now() + make_interval(secs => $3::double precision),
            last_error_kind = NULL
        FROM candidate
        WHERE batch.workspace_id = $1 AND batch.id = candidate.id
        RETURNING batch.id, batch.proof_kind, batch.schema_version,
                  batch.root_sha256, batch.leaf_count, batch.tree_algorithm,
                  batch.attempts, batch.lease_expires_at
        "#,
    )
    .bind(workspace_id)
    .bind(worker_id)
    .bind(lease_seconds)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await
    .map_err(ProofError::sqlx)?;
    tx.commit().await.map_err(ProofError::sqlx)?;
    let batches = row
        .into_iter()
        .map(|row| {
            Ok(RelayerBatch {
                id: row.id,
                proof_kind: row.proof_kind,
                schema_version: row.schema_version,
                root_sha256: encode_hash(&row.root_sha256)?,
                leaf_count: row.leaf_count,
                tree_algorithm: row.tree_algorithm,
                attempt: row.attempts,
                lease_expires_at: row.lease_expires_at,
            })
        })
        .collect::<Result<Vec<_>, ProofError>>()?;
    Ok(ClaimResponse { batches })
}
async fn confirm_batch(
    state: &crate::AppState,
    batch_id: Uuid,
    payload: ConfirmRequest,
    request_id_value: Option<&str>,
) -> Result<ProofBatchView, ProofError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let entry_id = payload.entry_uuid.to_ascii_lowercase();
    let log_id = payload.log_id.to_ascii_lowercase();
    let signer_fingerprint = payload.signer_fingerprint.to_ascii_lowercase();
    let signed_payload =
        hex::decode(&payload.payload_sha256).map_err(|_| ProofError::BadRequest)?;
    let integrated_at = OffsetDateTime::from_unix_timestamp(payload.integrated_time)
        .map_err(|_| ProofError::BadRequest)?;
    let receipt = json!({
        "canonicalized_body": &payload.canonicalized_body,
        "signed_entry_timestamp": &payload.signed_entry_timestamp,
        "inclusion_proof": &payload.inclusion_proof,
        "signature_base64": &payload.signature_base64,
        "public_key_pem": &payload.public_key_pem,
    });

    let mut tx = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(ProofError::sqlx)?;
    configure_transaction(&mut tx, state).await?;
    let existing = sqlx::query_as::<_, BatchRow>(
        r#"
        SELECT id, proof_kind, schema_version, hash_algorithm, tree_algorithm,
               root_sha256, leaf_count, status, attempts, max_attempts,
               available_at, anchor_kind, anchor_url, anchor_entry_id,
               anchor_sequence, anchor_integrated_at, anchor_log_id, anchor_receipt,
               signer_fingerprint, signed_payload_sha256, last_error_kind, lock_owner,
               created_at, updated_at, confirmed_at
        FROM external_proof_batches
        WHERE workspace_id = $1 AND id = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(batch_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(ProofError::sqlx)?
    .ok_or(ProofError::NotFound)?;

    let expected_payload = proof_anchor_payload(&existing)?;
    let expected_signed_payload: [u8; 32] = Sha256::digest(expected_payload.as_bytes()).into();
    if signed_payload.as_slice() != expected_signed_payload.as_ref() {
        return Err(ProofError::Conflict);
    }
    validate_rekor_receipt(&payload, expected_payload.as_bytes())?;

    if existing.status == "confirmed" {
        if existing.anchor_kind.as_deref() != Some(payload.anchor_kind.as_str())
            || existing.anchor_url.as_deref() != Some(payload.anchor_url.as_str())
            || existing.anchor_entry_id.as_deref() != Some(entry_id.as_str())
            || existing.anchor_sequence != Some(payload.log_index)
            || existing.anchor_integrated_at != Some(integrated_at)
            || existing.anchor_log_id.as_deref() != Some(log_id.as_str())
            || existing.anchor_receipt.as_ref() != Some(&receipt)
            || existing.signer_fingerprint.as_deref() != Some(signer_fingerprint.as_str())
            || existing.signed_payload_sha256.as_deref() != Some(signed_payload.as_slice())
        {
            return Err(ProofError::Conflict);
        }
        tx.commit().await.map_err(ProofError::sqlx)?;
        return ProofBatchView::try_from(existing);
    }
    if existing.status != "processing"
        || existing.anchor_kind.is_some()
        || existing.lock_owner.as_deref() != Some(payload.worker_id.as_str())
    {
        return Err(ProofError::Conflict);
    }

    sqlx::query(
        r#"
        UPDATE external_proof_batches
        SET status = 'confirmed', locked_at = NULL, lock_owner = NULL,
            lease_expires_at = NULL,
            anchor_kind = $3, anchor_url = $4, anchor_entry_id = $5,
            anchor_sequence = $6, anchor_integrated_at = $7, anchor_log_id = $8,
            anchor_receipt = $9, signer_fingerprint = $10,
            signed_payload_sha256 = $11, confirmed_at = now(),
            last_error_kind = NULL
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(batch_id)
    .bind(&payload.anchor_kind)
    .bind(&payload.anchor_url)
    .bind(&entry_id)
    .bind(payload.log_index)
    .bind(integrated_at)
    .bind(&log_id)
    .bind(&receipt)
    .bind(&signer_fingerprint)
    .bind(&signed_payload)
    .execute(&mut *tx)
    .await
    .map_err(ProofError::sqlx)?;

    append_audit(
        &mut tx,
        workspace_id,
        "external_proof.confirmed",
        batch_id,
        request_id_value,
        json!({
            "anchor_kind": &payload.anchor_kind,
            "anchor_url": &payload.anchor_url,
            "entry_id": &entry_id,
            "sequence": payload.log_index,
            "integrated_time": payload.integrated_time,
            "log_id": &log_id,
            "signer_fingerprint": &signer_fingerprint,
            "signed_payload_sha256": &payload.payload_sha256,
        }),
    )
    .await?;
    append_outbox(
        &mut tx,
        workspace_id,
        "proof.anchor.confirmed",
        batch_id,
        request_id_value,
        json!({
            "batch_id": batch_id,
            "proof_kind": existing.proof_kind,
            "root_sha256": encode_hash(&existing.root_sha256)?,
            "anchor_kind": &payload.anchor_kind,
            "anchor_url": &payload.anchor_url,
            "entry_id": &entry_id,
            "sequence": payload.log_index,
            "integrated_time": payload.integrated_time,
            "log_id": &log_id,
            "signer_fingerprint": &signer_fingerprint,
        }),
    )
    .await?;
    let batch = load_batch_tx(&mut tx, workspace_id, batch_id).await?;
    tx.commit().await.map_err(ProofError::sqlx)?;
    Ok(batch)
}

async fn fail_batch(
    state: &crate::AppState,
    batch_id: Uuid,
    payload: &FailRequest,
    request_id_value: Option<&str>,
) -> Result<ProofBatchView, ProofError> {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(ProofError::sqlx)?;
    configure_transaction(&mut tx, state).await?;
    #[derive(FromRow)]
    struct FailureState {
        attempts: i32,
        max_attempts: i32,
        proof_kind: String,
        root_sha256: Vec<u8>,
        status: String,
        lock_owner: Option<String>,
    }
    let current = sqlx::query_as::<_, FailureState>(
        r#"
        SELECT attempts, max_attempts, proof_kind, root_sha256, status, lock_owner
        FROM external_proof_batches
        WHERE workspace_id = $1 AND id = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(batch_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(ProofError::sqlx)?
    .ok_or(ProofError::NotFound)?;
    if current.status == "confirmed" {
        return Err(ProofError::Conflict);
    }
    if current.status != "processing"
        || current.lock_owner.as_deref() != Some(payload.worker_id.as_str())
    {
        return Err(ProofError::Conflict);
    }
    let dead = current.attempts >= current.max_attempts;
    let delay_seconds = retry_delay_seconds(current.attempts);
    sqlx::query(
        r#"
        UPDATE external_proof_batches
        SET status = CASE WHEN $3 THEN 'dead' ELSE 'failed' END,
            locked_at = NULL, lock_owner = NULL, lease_expires_at = NULL,
            available_at = CASE
                WHEN $3 THEN available_at
                ELSE now() + make_interval(secs => $4::double precision)
            END,
            last_error_kind = $5
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(batch_id)
    .bind(dead)
    .bind(delay_seconds)
    .bind(&payload.error_kind)
    .execute(&mut *tx)
    .await
    .map_err(ProofError::sqlx)?;
    if dead {
        append_outbox(
            &mut tx,
            workspace_id,
            "proof.anchor.dead",
            batch_id,
            request_id_value,
            json!({
                "batch_id": batch_id,
                "proof_kind": current.proof_kind,
                "root_sha256": encode_hash(&current.root_sha256)?,
                "attempts": current.attempts,
                "error_kind": payload.error_kind
            }),
        )
        .await?;
    }
    let batch = load_batch_tx(&mut tx, workspace_id, batch_id).await?;
    tx.commit().await.map_err(ProofError::sqlx)?;
    Ok(batch)
}
