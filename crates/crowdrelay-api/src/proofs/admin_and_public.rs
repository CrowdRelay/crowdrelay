pub async fn admin_list_batches(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Query(query): Query<ListBatchesQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(50);
    if !(1..=MAX_LIST_LIMIT).contains(&limit)
        || query.status.as_deref().is_some_and(|value| {
            !matches!(
                value,
                "queued" | "processing" | "confirmed" | "failed" | "dead"
            )
        })
    {
        return ProofError::BadRequest.into_response(request_id(&headers));
    }
    let future = async {
        let workspace_id = state.ticketing.workspace_id().into_uuid();
        let rows = if let Some(status) = query.status.as_deref() {
            sqlx::query_as::<_, BatchRow>(
                r#"
                SELECT id, proof_kind, schema_version, hash_algorithm, tree_algorithm,
                       root_sha256, leaf_count, status, attempts, max_attempts,
                       available_at, anchor_kind, anchor_url, anchor_entry_id,
                       anchor_sequence, anchor_integrated_at, anchor_log_id, anchor_receipt,
                       signer_fingerprint, signed_payload_sha256, last_error_kind, lock_owner, created_at, updated_at, confirmed_at
                FROM external_proof_batches
                WHERE workspace_id = $1 AND status = $2
                ORDER BY created_at DESC, id DESC
                LIMIT $3
                "#,
            )
            .bind(workspace_id)
            .bind(status)
            .bind(limit)
            .fetch_all(state.ticketing.pool())
            .await
            .map_err(ProofError::sqlx)?
        } else {
            sqlx::query_as::<_, BatchRow>(
                r#"
                SELECT id, proof_kind, schema_version, hash_algorithm, tree_algorithm,
                       root_sha256, leaf_count, status, attempts, max_attempts,
                       available_at, anchor_kind, anchor_url, anchor_entry_id,
                       anchor_sequence, anchor_integrated_at, anchor_log_id, anchor_receipt,
                       signer_fingerprint, signed_payload_sha256, last_error_kind, lock_owner, created_at, updated_at, confirmed_at
                FROM external_proof_batches
                WHERE workspace_id = $1
                ORDER BY created_at DESC, id DESC
                LIMIT $2
                "#,
            )
            .bind(workspace_id)
            .bind(limit)
            .fetch_all(state.ticketing.pool())
            .await
            .map_err(ProofError::sqlx)?
        };
        rows.into_iter()
            .map(ProofBatchView::try_from)
            .collect::<Result<Vec<ProofBatchView>, ProofError>>()
    };
    respond_private(run(&state, future).await, request_id(&headers))
}
pub async fn admin_create_audit_batch(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateAuditBatchRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return ProofError::BadRequest.into_response(request_id_value),
    };
    let limit = payload.limit.unwrap_or(1_024);
    if !(1..=MAX_AUDIT_BATCH).contains(&limit) {
        return ProofError::BadRequest.into_response(request_id_value);
    }
    let result = create_audit_batch(&state, &headers, limit);
    respond_private(run(&state, result).await, request_id_value)
}

pub async fn public_batch(
    State(state): State<crate::AppState>,
    Path(batch_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let future = load_batch(&state, batch_id);
    respond_public(run(&state, future).await, request_id(&headers))
}

pub async fn public_inclusion(
    State(state): State<crate::AppState>,
    Path((batch_id, source_kind, source_id)): Path<(Uuid, String, Uuid)>,
    headers: HeaderMap,
) -> Response {
    if !matches!(
        source_kind.as_str(),
        "audit_event" | "operator_action" | "reward_draw_run"
    ) {
        return ProofError::BadRequest.into_response(request_id(&headers));
    }
    let future = inclusion_proof(&state, batch_id, &source_kind, source_id);
    respond_public(run(&state, future).await, request_id(&headers))
}

pub async fn public_draw_status(
    State(state): State<crate::AppState>,
    Path(draw_slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    if draw_slug.is_empty() || draw_slug.len() > 128 {
        return ProofError::BadRequest.into_response(request_id(&headers));
    }
    let future = load_draw_status(&state, &draw_slug);
    respond_public_status(run(&state, future).await, request_id(&headers))
}

pub async fn public_draw(
    State(state): State<crate::AppState>,
    Path(draw_slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    if draw_slug.is_empty() || draw_slug.len() > 128 {
        return ProofError::BadRequest.into_response(request_id(&headers));
    }
    let future = load_draw_proof(&state, &draw_slug);
    respond_public(run(&state, future).await, request_id(&headers))
}

pub async fn internal_claim(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<ClaimRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return ProofError::BadRequest.into_response(request_id_value),
    };
    if !valid_worker_id(&payload.worker_id) {
        return ProofError::BadRequest.into_response(request_id_value);
    }
    let lease_seconds = payload.lease_seconds.unwrap_or(DEFAULT_LEASE_SECONDS);
    if !(30..=MAX_LEASE_SECONDS).contains(&lease_seconds) {
        return ProofError::BadRequest.into_response(request_id_value);
    }
    let limit = payload.limit.unwrap_or(MAX_CLAIM_BATCHES);
    if !(1..=MAX_CLAIM_BATCHES).contains(&limit) {
        return ProofError::BadRequest.into_response(request_id_value);
    }
    let future = claim_batches(&state, &payload.worker_id, lease_seconds, limit);
    respond_private(run(&state, future).await, request_id_value)
}

pub async fn internal_confirm(
    State(state): State<crate::AppState>,
    Path(batch_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<ConfirmRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return ProofError::BadRequest.into_response(request_id_value),
    };
    if !valid_confirm(&payload) {
        return ProofError::BadRequest.into_response(request_id_value);
    }
    let future = confirm_batch(&state, batch_id, payload, request_id_value.as_deref());
    respond_private(run(&state, future).await, request_id_value)
}

pub async fn internal_fail(
    State(state): State<crate::AppState>,
    Path(batch_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<FailRequest>, JsonRejection>,
) -> Response {
    let request_id_value = request_id(&headers);
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return ProofError::BadRequest.into_response(request_id_value),
    };
    if !valid_worker_id(&payload.worker_id)
        || payload.error_kind.is_empty()
        || payload.error_kind.len() > 96
        || !payload
            .error_kind
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return ProofError::BadRequest.into_response(request_id_value);
    }
    let future = fail_batch(&state, batch_id, &payload, request_id_value.as_deref());
    respond_private(run(&state, future).await, request_id_value)
}

async fn create_audit_batch(
    state: &crate::AppState,
    headers: &HeaderMap,
    limit: i64,
) -> Result<AuditBatchResult, ProofError> {
    let idempotency_key = idempotency_key(headers)?;
    let request_id_value = request_id(headers);
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let mut tx = state
        .ticketing
        .pool()
        .begin()
        .await
        .map_err(ProofError::sqlx)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("{workspace_id}:external-proof-audit"))
        .execute(&mut *tx)
        .await
        .map_err(ProofError::sqlx)?;

    if let Some(existing) = sqlx::query_as::<_, ExistingAction>(
        r#"
        SELECT action, target_id, details
        FROM operator_actions
        WHERE workspace_id = $1 AND idempotency_key = $2
        "#,
    )
    .bind(workspace_id)
    .bind(&idempotency_key)
    .fetch_optional(&mut *tx)
    .await
    .map_err(ProofError::sqlx)?
    {
        if existing.action != "external_proof.audit_batch_created"
            || existing.details.get("limit").and_then(Value::as_i64) != Some(limit)
        {
            return Err(ProofError::Conflict);
        }
        if existing.details.get("empty").and_then(Value::as_bool) == Some(true) {
            tx.commit().await.map_err(ProofError::sqlx)?;
            return Ok(AuditBatchResult {
                batch: None,
                replayed: true,
            });
        }
        let batch = load_batch_tx(&mut tx, workspace_id, existing.target_id).await?;
        tx.commit().await.map_err(ProofError::sqlx)?;
        return Ok(AuditBatchResult {
            batch: Some(batch),
            replayed: true,
        });
    }

    let rows = sqlx::query_as::<_, LedgerRow>(
        r#"
        WITH candidates AS (
            SELECT 'audit_event'::text AS source_kind,
                   audit.id AS source_id,
                   audit.occurred_at,
                   jsonb_build_array(
                       'crowdrelay/audit-event/v1', audit.id, audit.actor_kind,
                       audit.actor_member_id, audit.action, audit.target_type,
                       audit.target_id, audit.request_id, audit.metadata,
                       audit.occurred_at
                   )::text AS canonical
            FROM audit_events AS audit
            WHERE audit.workspace_id = $1
              AND audit.action NOT LIKE 'external_proof.%'
              AND NOT EXISTS (
                  SELECT 1 FROM external_proof_items AS item
                  WHERE item.workspace_id = audit.workspace_id
                    AND item.source_kind = 'audit_event'
                    AND item.source_id = audit.id
              )
            UNION ALL
            SELECT 'operator_action'::text,
                   action.id,
                   action.created_at,
                   jsonb_build_array(
                       'crowdrelay/operator-action/v1', action.id, action.action,
                       action.target_type, action.target_id, action.actor_type,
                       action.idempotency_key, action.request_id, action.details,
                       action.created_at
                   )::text
            FROM operator_actions AS action
            WHERE action.workspace_id = $1
              AND action.action NOT LIKE 'external_proof.%'
              AND NOT EXISTS (
                  SELECT 1 FROM external_proof_items AS item
                  WHERE item.workspace_id = action.workspace_id
                    AND item.source_kind = 'operator_action'
                    AND item.source_id = action.id
              )
        )
        SELECT source_kind, source_id, occurred_at, canonical
        FROM candidates
        ORDER BY occurred_at, source_kind, source_id
        LIMIT $2
        "#,
    )
    .bind(workspace_id)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await
    .map_err(ProofError::sqlx)?;

    if rows.is_empty() {
        let no_op_id = Uuid::now_v7();
        append_operator_action(
            &mut tx,
            workspace_id,
            "external_proof.audit_batch_created",
            no_op_id,
            &idempotency_key,
            request_id_value.as_deref(),
            json!({"limit": limit, "empty": true}),
        )
        .await?;
        append_audit(
            &mut tx,
            workspace_id,
            "external_proof.audit_batch_empty",
            no_op_id,
            request_id_value.as_deref(),
            json!({"limit": limit}),
        )
        .await?;
        tx.commit().await.map_err(ProofError::sqlx)?;
        return Ok(AuditBatchResult {
            batch: None,
            replayed: false,
        });
    }

    let leaves: Vec<[u8; 32]> = rows
        .iter()
        .map(|row| leaf_hash(row.canonical.as_bytes()))
        .collect();
    let root = merkle_root(&leaves).ok_or(ProofError::Unexpected)?;
    let batch_id = Uuid::now_v7();
    let leaf_count = i32::try_from(leaves.len()).map_err(|_| ProofError::Unexpected)?;

    sqlx::query(
        r#"
        INSERT INTO external_proof_batches (
            id, workspace_id, proof_kind, schema_version, tree_algorithm,
            root_sha256, leaf_count, request_id
        ) VALUES ($1, $2, 'audit_ledger', 1, 'binary-duplicate-last-v1', $3, $4, $5)
        "#,
    )
    .bind(batch_id)
    .bind(workspace_id)
    .bind(root.to_vec())
    .bind(leaf_count)
    .bind(request_id_value.as_deref())
    .execute(&mut *tx)
    .await
    .map_err(ProofError::sqlx)?;

    let sequences: Vec<i32> = (0..leaf_count).collect();
    let source_kinds: Vec<String> = rows.iter().map(|row| row.source_kind.clone()).collect();
    let source_ids: Vec<Uuid> = rows.iter().map(|row| row.source_id).collect();
    let leaf_values: Vec<Vec<u8>> = leaves.iter().map(|leaf| leaf.to_vec()).collect();
    let occurred: Vec<OffsetDateTime> = rows.iter().map(|row| row.occurred_at).collect();
    sqlx::query(
        r#"
        INSERT INTO external_proof_items (
            workspace_id, batch_id, sequence, source_kind,
            source_id, leaf_sha256, occurred_at
        )
        SELECT $1, $2, item.sequence, item.source_kind,
               item.source_id, item.leaf_sha256, item.occurred_at
        FROM unnest(
            $3::integer[], $4::text[], $5::uuid[], $6::bytea[], $7::timestamptz[]
        ) AS item(sequence, source_kind, source_id, leaf_sha256, occurred_at)
        "#,
    )
    .bind(workspace_id)
    .bind(batch_id)
    .bind(sequences)
    .bind(source_kinds)
    .bind(source_ids)
    .bind(leaf_values)
    .bind(occurred)
    .execute(&mut *tx)
    .await
    .map_err(ProofError::sqlx)?;

    append_operator_action(
        &mut tx,
        workspace_id,
        "external_proof.audit_batch_created",
        batch_id,
        &idempotency_key,
        request_id_value.as_deref(),
        json!({"limit": limit, "empty": false, "leaf_count": leaf_count, "root_sha256": hex::encode(root)}),
    )
    .await?;
    append_audit(
        &mut tx,
        workspace_id,
        "external_proof.audit_batch_created",
        batch_id,
        request_id_value.as_deref(),
        json!({"leaf_count": leaf_count, "root_sha256": hex::encode(root)}),
    )
    .await?;
    tx.commit().await.map_err(ProofError::sqlx)?;
    let batch = load_batch(state, batch_id).await?;
    Ok(AuditBatchResult {
        batch: Some(batch),
        replayed: false,
    })
}
