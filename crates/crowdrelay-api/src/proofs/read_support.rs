async fn load_batch(state: &crate::AppState, batch_id: Uuid) -> Result<ProofBatchView, ProofError> {
    let row = load_batch_row(state, batch_id).await?;
    ProofBatchView::try_from(row)
}

async fn load_batch_row(state: &crate::AppState, batch_id: Uuid) -> Result<BatchRow, ProofError> {
    sqlx::query_as::<_, BatchRow>(
        r#"
        SELECT id, proof_kind, schema_version, hash_algorithm, tree_algorithm,
               root_sha256, leaf_count, status, attempts, max_attempts,
               available_at, anchor_kind, anchor_url, anchor_entry_id,
                       anchor_sequence, anchor_integrated_at, anchor_log_id, anchor_receipt,
                       signer_fingerprint, signed_payload_sha256, last_error_kind, lock_owner,
               created_at, updated_at, confirmed_at
        FROM external_proof_batches
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(batch_id)
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(ProofError::sqlx)?
    .ok_or(ProofError::NotFound)
}

async fn load_batch_tx(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    batch_id: Uuid,
) -> Result<ProofBatchView, ProofError> {
    let row = sqlx::query_as::<_, BatchRow>(
        r#"
        SELECT id, proof_kind, schema_version, hash_algorithm, tree_algorithm,
               root_sha256, leaf_count, status, attempts, max_attempts,
               available_at, anchor_kind, anchor_url, anchor_entry_id,
                       anchor_sequence, anchor_integrated_at, anchor_log_id, anchor_receipt,
                       signer_fingerprint, signed_payload_sha256, last_error_kind, lock_owner,
               created_at, updated_at, confirmed_at
        FROM external_proof_batches
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(batch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(ProofError::sqlx)?
    .ok_or(ProofError::NotFound)?;
    ProofBatchView::try_from(row)
}

async fn inclusion_proof(
    state: &crate::AppState,
    batch_id: Uuid,
    source_kind: &str,
    source_id: Uuid,
) -> Result<InclusionProof, ProofError> {
    let batch = load_batch(state, batch_id).await?;
    let items = sqlx::query_as::<_, ProofItemRow>(
        r#"
        SELECT sequence, source_kind, source_id, leaf_sha256
        FROM external_proof_items
        WHERE workspace_id = $1 AND batch_id = $2
        ORDER BY sequence
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(batch_id)
    .fetch_all(state.ticketing.pool())
    .await
    .map_err(ProofError::sqlx)?;
    let target = items
        .iter()
        .position(|item| item.source_kind == source_kind && item.source_id == source_id)
        .ok_or(ProofError::NotFound)?;
    let leaves: Vec<[u8; 32]> = items
        .iter()
        .map(|item| hash_array(&item.leaf_sha256))
        .collect::<Result<_, _>>()?;
    let root = merkle_root(&leaves).ok_or(ProofError::Unexpected)?;
    let expected_root = decode_hash(&batch.root_sha256)?;
    let proof = merkle_path(&leaves, target)?;
    let leaf = leaves.get(target).copied().ok_or(ProofError::Unexpected)?;
    let sequence = items
        .get(target)
        .map(|item| item.sequence)
        .ok_or(ProofError::Unexpected)?;
    let verified = verify_path(leaf, target, &proof, expected_root) && root == expected_root;
    Ok(InclusionProof {
        batch,
        source_kind: source_kind.to_owned(),
        source_id,
        sequence,
        leaf_sha256: hex::encode(leaf),
        proof: proof
            .into_iter()
            .map(|step| MerkleStep {
                side: if step.sibling_left { "left" } else { "right" },
                sha256: hex::encode(step.hash),
            })
            .collect(),
        verified,
    })
}

async fn load_draw_status(
    state: &crate::AppState,
    draw_slug: &str,
) -> Result<PublicDrawStatus, ProofError> {
    let row = sqlx::query_as::<_, DrawStatusRow>(
        r#"
        SELECT draw.slug AS draw_slug,
               draw.name AS draw_name,
               draw.status,
               draw.draw_at,
               draw.completed_at,
               EXISTS (
                   SELECT 1
                   FROM reward_draw_proofs AS proof
                   WHERE proof.workspace_id = draw.workspace_id
                     AND proof.draw_id = draw.id
               ) AS proof_available
        FROM reward_draws AS draw
        WHERE draw.workspace_id = $1 AND draw.slug = $2
        LIMIT 1
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(draw_slug)
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(ProofError::sqlx)?
    .ok_or(ProofError::NotFound)?;

    Ok(PublicDrawStatus {
        schema: "crowdrelay/draw-status/v1",
        draw_slug: row.draw_slug,
        draw_name: row.draw_name,
        status: row.status,
        draw_at: row.draw_at,
        completed_at: row.completed_at,
        proof_available: row.proof_available,
    })
}

async fn load_draw_proof(
    state: &crate::AppState,
    draw_slug: &str,
) -> Result<PublicDrawProof, ProofError> {
    let row = sqlx::query_as::<_, DrawProofRow>(
        r#"
        SELECT draw.slug AS draw_slug,
               draw.name AS draw_name,
               run.id AS run_id,
               run.algorithm_version,
               run.seed_hash,
               run.revealed_seed_hex,
               run.eligible_count,
               run.total_entries,
               run.requested_winners,
               run.selected_winners,
               proof.receipt_sha256,
               proof.candidate_snapshot_sha256,
               proof.winner_snapshot_sha256,
               batch.id AS batch_id,
               batch.status AS batch_status,
               batch.anchor_kind,
               batch.anchor_url,
               batch.anchor_entry_id,
               batch.anchor_sequence,
               batch.anchor_integrated_at,
               batch.anchor_log_id,
               batch.anchor_receipt,
               batch.signer_fingerprint,
               batch.signed_payload_sha256,
               batch.confirmed_at,
               run.completed_at
        FROM reward_draws AS draw
        JOIN reward_draw_runs AS run
          ON run.workspace_id = draw.workspace_id AND run.draw_id = draw.id
        JOIN reward_draw_proofs AS proof
          ON proof.workspace_id = run.workspace_id AND proof.run_id = run.id
        JOIN external_proof_batches AS batch
          ON batch.workspace_id = proof.workspace_id AND batch.id = proof.anchor_batch_id
        WHERE draw.workspace_id = $1 AND draw.slug = $2
          AND run.status = 'completed'
        ORDER BY run.completed_at DESC, run.id DESC
        LIMIT 1
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .bind(draw_slug)
    .fetch_optional(state.ticketing.pool())
    .await
    .map_err(ProofError::sqlx)?
    .ok_or(ProofError::NotFound)?;

    let receipt = draw_receipt_hash(
        row.run_id,
        &row.algorithm_version,
        &row.seed_hash,
        &row.revealed_seed_hex,
        row.eligible_count,
        row.total_entries,
        row.requested_winners,
        row.selected_winners,
        &row.candidate_snapshot_sha256,
        &row.winner_snapshot_sha256,
    )?;
    let stored = hash_array(&row.receipt_sha256)?;
    Ok(PublicDrawProof {
        schema: "crowdrelay/draw-receipt/v1",
        draw_slug: row.draw_slug,
        draw_name: row.draw_name,
        run_id: row.run_id,
        algorithm_version: row.algorithm_version,
        seed_hash_sha256: encode_hash(&row.seed_hash)?,
        revealed_seed_hex: row.revealed_seed_hex,
        eligible_count: row.eligible_count,
        total_entries: row.total_entries,
        requested_winners: row.requested_winners,
        selected_winners: row.selected_winners,
        candidate_snapshot_sha256: encode_hash(&row.candidate_snapshot_sha256)?,
        winner_snapshot_sha256: encode_hash(&row.winner_snapshot_sha256)?,
        receipt_sha256: hex::encode(stored),
        locally_verified: receipt == stored,
        anchor: PublicAnchor {
            batch_id: row.batch_id,
            status: row.batch_status,
            anchor_kind: row.anchor_kind,
            anchor_url: row.anchor_url,
            entry_id: row.anchor_entry_id,
            sequence: row.anchor_sequence,
            integrated_at: row.anchor_integrated_at,
            log_id: row.anchor_log_id,
            receipt: row.anchor_receipt,
            signer_fingerprint: row.signer_fingerprint,
            signed_payload_sha256: row
                .signed_payload_sha256
                .as_deref()
                .map(encode_hash)
                .transpose()?,
            confirmed_at: row.confirmed_at,
        },
        completed_at: row.completed_at,
    })
}
