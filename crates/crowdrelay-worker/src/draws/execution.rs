async fn execute_draw(
    transaction: &mut Transaction<'_, Postgres>,
    draw: &DrawRow,
) -> Result<DrawSummary, DrawWorkerError> {
    validate_draw(draw)?;

    let mut seed = [0_u8; 32];
    fill_random(&mut seed).map_err(|_| DrawWorkerError::Entropy)?;
    let seed_hash = Sha256::digest(seed).to_vec();
    let run_id = Uuid::now_v7();

    sqlx::query(
        r#"
        INSERT INTO reward_draw_runs (
            id, workspace_id, draw_id, algorithm_version, seed_hash,
            requested_winners, status
        )
        VALUES ($1, $2, $3, $4, $5, $6, 'running')
        "#,
    )
    .bind(run_id)
    .bind(draw.workspace_id)
    .bind(draw.id)
    .bind(ALGORITHM_VERSION)
    .bind(&seed_hash)
    .bind(draw.winner_count)
    .execute(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;

    let raw_candidates = load_candidates(transaction, draw).await?;
    let mut candidates = rank_candidates(raw_candidates, draw, &seed)?;
    candidates.sort_by(compare_candidates);

    let total_entries = candidates.iter().try_fold(0_i64, |total, candidate| {
        total
            .checked_add(i64::from(candidate.entry_count))
            .ok_or(DrawWorkerError::Arithmetic)
    })?;

    persist_candidates(transaction, draw, run_id, &candidates).await?;

    let selected = match draw.prize_kind.as_str() {
        "admission_pass" => issue_admission_winners(transaction, draw, run_id, &candidates).await?,
        "physical_item" => issue_physical_winners(transaction, draw, run_id, &candidates).await?,
        _ => return Err(DrawWorkerError::InvalidDraw),
    };

    let eligible_count =
        i32::try_from(candidates.len()).map_err(|_| DrawWorkerError::Arithmetic)?;
    let selected_winners = i32::try_from(selected).map_err(|_| DrawWorkerError::Arithmetic)?;
    let revealed_seed = hex::encode(seed);

    sqlx::query(
        r#"
        UPDATE reward_draw_runs
        SET eligible_count = $4,
            total_entries = $5,
            selected_winners = $6,
            status = 'completed',
            revealed_seed_hex = $7,
            completed_at = now()
        WHERE workspace_id = $1 AND draw_id = $2 AND id = $3
        "#,
    )
    .bind(draw.workspace_id)
    .bind(draw.id)
    .bind(run_id)
    .bind(eligible_count)
    .bind(total_entries)
    .bind(selected_winners)
    .bind(&revealed_seed)
    .execute(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;

    sqlx::query(
        "UPDATE reward_draws SET status = 'completed', completed_at = now(), last_error = NULL WHERE workspace_id = $1 AND id = $2",
    )
    .bind(draw.workspace_id)
    .bind(draw.id)
    .execute(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;

    persist_external_draw_proof(
        transaction,
        draw,
        run_id,
        &candidates,
        &seed_hash,
        &revealed_seed,
        eligible_count,
        total_entries,
        selected_winners,
    )
    .await?;

    append_outbox(
        transaction,
        draw.workspace_id,
        "reward_draw.completed",
        &format!("draw:{}:run:{}", draw.id, run_id),
        json!({
            "draw_id": draw.id,
            "draw_slug": draw.slug,
            "draw_name": draw.name,
            "run_id": run_id,
            "algorithm_version": ALGORITHM_VERSION,
            "seed_hash": hex::encode(&seed_hash),
            "revealed_seed": revealed_seed,
            "eligible_count": eligible_count,
            "total_entries": total_entries,
            "requested_winners": draw.winner_count,
            "selected_winners": selected_winners,
        }),
    )
    .await?;

    append_audit(
        transaction,
        draw.workspace_id,
        "reward_draw.completed",
        "reward_draw",
        draw.id,
        json!({
            "run_id": run_id,
            "algorithm_version": ALGORITHM_VERSION,
            "seed_hash": hex::encode(seed_hash),
            "eligible_count": eligible_count,
            "total_entries": total_entries,
            "selected_winners": selected_winners,
        }),
    )
    .await?;

    Ok(DrawSummary {
        eligible_count,
        total_entries,
        selected_winners,
    })
}
#[allow(clippy::too_many_arguments)]
async fn persist_external_draw_proof(
    transaction: &mut Transaction<'_, Postgres>,
    draw: &DrawRow,
    run_id: Uuid,
    candidates: &[RankedCandidate],
    seed_hash: &[u8],
    revealed_seed: &str,
    eligible_count: i32,
    total_entries: i64,
    selected_winners: i32,
) -> Result<(), DrawWorkerError> {
    let enabled = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT COALESCE(
            bool_or(enabled) FILTER (WHERE key = 'draw_proofs_enabled'),
            true
        )
        FROM ecosystem_feature_flags
        WHERE workspace_id = $1 AND key = 'draw_proofs_enabled'
        "#,
    )
    .bind(draw.workspace_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;
    if !enabled {
        return Ok(());
    }

    let candidate_digest = candidate_snapshot_digest(run_id, candidates);
    let winner_digest =
        winner_snapshot_digest(transaction, draw.workspace_id, run_id, selected_winners).await?;
    let receipt = draw_receipt_digest(
        run_id,
        ALGORITHM_VERSION,
        seed_hash,
        revealed_seed,
        eligible_count,
        total_entries,
        draw.winner_count,
        selected_winners,
        candidate_digest,
        winner_digest,
    )?;
    let batch_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO external_proof_batches (
            id, workspace_id, proof_kind, schema_version, tree_algorithm,
            root_sha256, leaf_count, request_id
        ) VALUES ($1, $2, 'draw_receipt', 1, 'single-leaf-v1', $3, 1, $4)
        "#,
    )
    .bind(batch_id)
    .bind(draw.workspace_id)
    .bind(receipt.to_vec())
    .bind(format!("draw:{run_id}:proof"))
    .execute(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;
    sqlx::query(
        r#"
        INSERT INTO external_proof_items (
            workspace_id, batch_id, sequence, source_kind,
            source_id, leaf_sha256, occurred_at
        ) VALUES ($1, $2, 0, 'reward_draw_run', $3, $4, now())
        "#,
    )
    .bind(draw.workspace_id)
    .bind(batch_id)
    .bind(run_id)
    .bind(receipt.to_vec())
    .execute(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;
    sqlx::query(
        r#"
        INSERT INTO reward_draw_proofs (
            workspace_id, run_id, draw_id, anchor_batch_id,
            receipt_sha256, candidate_snapshot_sha256,
            winner_snapshot_sha256, eligible_count, selected_winners
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
    )
    .bind(draw.workspace_id)
    .bind(run_id)
    .bind(draw.id)
    .bind(batch_id)
    .bind(receipt.to_vec())
    .bind(candidate_digest.to_vec())
    .bind(winner_digest.to_vec())
    .bind(eligible_count)
    .bind(selected_winners)
    .execute(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;

    Ok(())
}

fn candidate_snapshot_digest(run_id: Uuid, candidates: &[RankedCandidate]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"crowdrelay/draw-candidates/v1\0");
    hasher.update((candidates.len() as u64).to_be_bytes());
    for candidate in candidates {
        hasher.update(candidate_alias(run_id, candidate.fan_id));
        hasher.update(candidate.qualified_referrals.to_be_bytes());
        hasher.update(candidate.concert_checkins.to_be_bytes());
        hasher.update(candidate.checkin_entries.to_be_bytes());
        hasher.update(candidate.entry_count.to_be_bytes());
        hasher.update(candidate.selection_score.to_bits().to_be_bytes());
    }
    hasher.finalize().into()
}

async fn winner_snapshot_digest(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    run_id: Uuid,
    expected_count: i32,
) -> Result<[u8; 32], DrawWorkerError> {
    let winners = sqlx::query_as::<_, (i32, Uuid, i32, f64)>(
        r#"
        SELECT winner.winner_rank, candidate.fan_id,
               candidate.entry_count, candidate.selection_score
        FROM reward_draw_winners AS winner
        JOIN reward_draw_candidates AS candidate
          ON candidate.workspace_id = winner.workspace_id
         AND candidate.run_id = winner.run_id
         AND candidate.fan_id = winner.fan_id
        WHERE winner.workspace_id = $1 AND winner.run_id = $2
        ORDER BY winner.winner_rank
        "#,
    )
    .bind(workspace_id)
    .bind(run_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;
    let actual_count = i32::try_from(winners.len()).map_err(|_| DrawWorkerError::Arithmetic)?;
    if actual_count != expected_count {
        return Err(DrawWorkerError::InvalidDraw);
    }

    let mut hasher = Sha256::new();
    hasher.update(b"crowdrelay/draw-winners/v1\0");
    hasher.update((winners.len() as u64).to_be_bytes());
    for (index, (winner_rank, fan_id, entry_count, selection_score)) in
        winners.into_iter().enumerate()
    {
        let expected_rank = i32::try_from(index + 1).map_err(|_| DrawWorkerError::Arithmetic)?;
        if winner_rank != expected_rank {
            return Err(DrawWorkerError::InvalidDraw);
        }
        hasher.update((winner_rank as u64).to_be_bytes());
        hasher.update(candidate_alias(run_id, fan_id));
        hasher.update(entry_count.to_be_bytes());
        hasher.update(selection_score.to_bits().to_be_bytes());
    }
    Ok(hasher.finalize().into())
}

fn candidate_alias(run_id: Uuid, fan_id: Uuid) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"crowdrelay/draw-candidate-alias/v1\0");
    hasher.update(run_id.as_bytes());
    hasher.update(fan_id.as_bytes());
    hasher.finalize().into()
}

#[allow(clippy::too_many_arguments)]
fn draw_receipt_digest(
    run_id: Uuid,
    algorithm_version: &str,
    seed_hash: &[u8],
    revealed_seed: &str,
    eligible_count: i32,
    total_entries: i64,
    requested_winners: i32,
    selected_winners: i32,
    candidate_digest: [u8; 32],
    winner_digest: [u8; 32],
) -> Result<[u8; 32], DrawWorkerError> {
    if seed_hash.len() != 32 {
        return Err(DrawWorkerError::InvalidDraw);
    }
    let mut hasher = Sha256::new();
    hasher.update(b"crowdrelay/draw-receipt/v1\0");
    hasher.update(run_id.as_bytes());
    update_proof_field(&mut hasher, algorithm_version.as_bytes());
    hasher.update(seed_hash);
    update_proof_field(&mut hasher, revealed_seed.as_bytes());
    hasher.update(eligible_count.to_be_bytes());
    hasher.update(total_entries.to_be_bytes());
    hasher.update(requested_winners.to_be_bytes());
    hasher.update(selected_winners.to_be_bytes());
    hasher.update(candidate_digest);
    hasher.update(winner_digest);
    Ok(hasher.finalize().into())
}

fn update_proof_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}
