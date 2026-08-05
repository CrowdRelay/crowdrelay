//! Durable referral-weighted reward drawing.
//!
//! Each due draw is processed in one PostgreSQL transaction. Candidate weights
//! are snapshotted, selection is reproducible from the revealed seed, winners
//! are persisted exactly once, and fulfillment enters the transactional outbox.

use std::{cmp::Ordering, time::Duration};

use getrandom::fill as fill_random;
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

const ALGORITHM_VERSION: &str = "hmac-sha256-exp-race-v1";
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(15);
const MAX_DRAWS_PER_TICK: usize = 16;
const MAX_ERROR_CHARS: usize = 500;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Debug)]
pub struct WeightedDrawWorkerConfig {
    pub poll_interval: Duration,
    pub operation_timeout: Duration,
    pub lock_timeout: Duration,
}

impl WeightedDrawWorkerConfig {
    #[must_use]
    pub const fn with_database_timeouts(
        operation_timeout: Duration,
        lock_timeout: Duration,
    ) -> Self {
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            operation_timeout,
            lock_timeout,
        }
    }
}

#[derive(Clone)]
pub struct WeightedDrawWorker {
    pool: PgPool,
    config: WeightedDrawWorkerConfig,
}

impl WeightedDrawWorker {
    pub fn new(pool: PgPool, config: WeightedDrawWorkerConfig) -> Result<Self, DrawWorkerError> {
        if config.poll_interval.is_zero()
            || config.operation_timeout.is_zero()
            || config.lock_timeout.is_zero()
        {
            return Err(DrawWorkerError::InvalidConfiguration);
        }
        Ok(Self { pool, config })
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = interval(self.config.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    if let Err(error) = self.process_due_draws().await {
                        tracing::error!(error = %error, "weighted draw processing failed");
                    }
                }
            }
        }
    }

    async fn process_due_draws(&self) -> Result<(), DrawWorkerError> {
        for _ in 0..MAX_DRAWS_PER_TICK {
            let processed = timeout(self.config.operation_timeout, self.process_one())
                .await
                .map_err(|_| DrawWorkerError::TimedOut)??;
            if !processed {
                break;
            }
        }
        Ok(())
    }

    async fn process_one(&self) -> Result<bool, DrawWorkerError> {
        let mut transaction = self.pool.begin().await.map_err(DrawWorkerError::sqlx)?;
        configure_transaction(&mut transaction, &self.config).await?;

        let Some(draw) = lock_due_draw(&mut transaction).await? else {
            transaction.commit().await.map_err(DrawWorkerError::sqlx)?;
            return Ok(false);
        };

        let result = execute_draw(&mut transaction, &draw).await;
        match result {
            Ok(summary) => {
                transaction.commit().await.map_err(DrawWorkerError::sqlx)?;
                tracing::info!(
                    draw_id = %draw.id,
                    draw_slug = %draw.slug,
                    eligible = summary.eligible_count,
                    winners = summary.selected_winners,
                    total_entries = summary.total_entries,
                    "weighted reward draw completed"
                );
                Ok(true)
            }
            Err(error) => {
                transaction
                    .rollback()
                    .await
                    .map_err(DrawWorkerError::sqlx)?;
                record_failure(&self.pool, draw.id, &error, &self.config).await?;
                Err(error)
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum DrawWorkerError {
    #[error("weighted draw worker configuration is invalid")]
    InvalidConfiguration,
    #[error("weighted draw database operation timed out")]
    TimedOut,
    #[error("weighted draw database operation failed")]
    Database,
    #[error("weighted draw configuration is inconsistent")]
    InvalidDraw,
    #[error("secure random seed generation failed")]
    Entropy,
    #[error("weighted draw arithmetic overflowed")]
    Arithmetic,
}

impl DrawWorkerError {
    fn sqlx(_: sqlx::Error) -> Self {
        Self::Database
    }
}

#[derive(Debug, FromRow)]
struct DrawRow {
    id: Uuid,
    workspace_id: Uuid,
    slug: String,
    name: String,
    prize_kind: String,
    eligibility_kind: String,
    event_id: Option<Uuid>,
    admission_pool_id: Option<Uuid>,
    reward_rule_id: Option<Uuid>,
    winner_count: i32,
    base_entries: i32,
    entries_per_referral: i32,
    entries_per_checkin: i32,
    max_entries: i32,
    claim_expires_hours: i32,
    closes_at: OffsetDateTime,
    opens_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct CandidateRow {
    fan_id: Uuid,
    normalized_email: String,
    display_name: Option<String>,
    qualified_referrals: i64,
    concert_checkins: i64,
}

#[derive(Clone, Debug)]
struct RankedCandidate {
    fan_id: Uuid,
    normalized_email: String,
    display_name: Option<String>,
    qualified_referrals: i32,
    concert_checkins: i32,
    checkin_entries: i32,
    entry_count: i32,
    selection_score: f64,
}

#[derive(Debug, FromRow)]
struct AdmissionPoolRow {
    id: Uuid,
    capacity: i32,
    issued_count: i32,
    reserved_count: i32,
}

#[derive(Debug, FromRow)]
struct PhysicalRuleRow {
    id: Uuid,
    item_name: String,
    sku: String,
    expires_days: i32,
}

#[derive(Debug)]
struct DrawSummary {
    eligible_count: i32,
    total_entries: i64,
    selected_winners: i32,
}

async fn configure_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    config: &WeightedDrawWorkerConfig,
) -> Result<(), DrawWorkerError> {
    let statement_ms = duration_milliseconds(config.operation_timeout)?;
    let lock_ms = duration_milliseconds(config.lock_timeout)?;
    sqlx::query(
        "SELECT set_config('statement_timeout', $1, true), set_config('lock_timeout', $2, true)",
    )
    .bind(format!("{statement_ms}ms"))
    .bind(format!("{lock_ms}ms"))
    .execute(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;
    Ok(())
}

fn duration_milliseconds(value: Duration) -> Result<u128, DrawWorkerError> {
    let milliseconds = value.as_millis();
    if milliseconds == 0 || milliseconds > 2_147_483_647_u128 {
        return Err(DrawWorkerError::InvalidConfiguration);
    }
    Ok(milliseconds)
}

async fn lock_due_draw(
    transaction: &mut Transaction<'_, Postgres>,
) -> Result<Option<DrawRow>, DrawWorkerError> {
    let row = sqlx::query_as::<_, DrawRow>(
        r#"
        SELECT
            id, workspace_id, slug, name, prize_kind, eligibility_kind,
            event_id, admission_pool_id, reward_rule_id, winner_count,
            base_entries, entries_per_referral, entries_per_checkin, max_entries,
            claim_expires_hours, closes_at, opens_at
        FROM reward_draws
        WHERE status = 'scheduled'
          AND draw_at <= now()
          AND closes_at <= now()
        ORDER BY draw_at, id
        FOR UPDATE SKIP LOCKED
        LIMIT 1
        "#,
    )
    .fetch_optional(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;

    if let Some(draw) = &row {
        sqlx::query(
            "UPDATE reward_draws SET status = 'running', attempts = attempts + 1, last_error = NULL WHERE workspace_id = $1 AND id = $2",
        )
        .bind(draw.workspace_id)
        .bind(draw.id)
        .execute(&mut **transaction)
        .await
        .map_err(DrawWorkerError::sqlx)?;
    }

    Ok(row)
}

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

fn validate_draw(draw: &DrawRow) -> Result<(), DrawWorkerError> {
    if draw.winner_count <= 0
        || draw.base_entries <= 0
        || draw.entries_per_referral < 0
        || draw.entries_per_checkin < 0
        || draw.max_entries < draw.base_entries
        || draw.claim_expires_hours <= 0
        || !matches!(
            draw.eligibility_kind.as_str(),
            "all_active" | "event_interest"
        )
    {
        return Err(DrawWorkerError::InvalidDraw);
    }

    match draw.prize_kind.as_str() {
        "admission_pass"
            if draw.event_id.is_some()
                && draw.admission_pool_id.is_some()
                && draw.reward_rule_id.is_none() => {}
        "physical_item" if draw.admission_pool_id.is_none() && draw.reward_rule_id.is_some() => {}
        _ => return Err(DrawWorkerError::InvalidDraw),
    }

    if draw.eligibility_kind == "event_interest" && draw.event_id.is_none() {
        return Err(DrawWorkerError::InvalidDraw);
    }
    Ok(())
}

async fn load_candidates(
    transaction: &mut Transaction<'_, Postgres>,
    draw: &DrawRow,
) -> Result<Vec<CandidateRow>, DrawWorkerError> {
    sqlx::query_as::<_, CandidateRow>(
        r#"
        SELECT
            fan.id AS fan_id,
            fan.normalized_email,
            fan.display_name,
            referral_count.qualified_referrals,
            checkin_count.concert_checkins
        FROM fans AS fan
        CROSS JOIN LATERAL (
            SELECT count(*)::bigint AS qualified_referrals
            FROM referral_attributions AS attribution
            WHERE attribution.workspace_id = fan.workspace_id
              AND attribution.referrer_fan_id = fan.id
              AND attribution.status = 'qualified'
              AND attribution.qualified_at <= $4
        ) AS referral_count
        CROSS JOIN LATERAL (
            SELECT count(*)::bigint AS concert_checkins
            FROM concert_checkins AS checkin
            WHERE checkin.workspace_id = fan.workspace_id
              AND checkin.fan_id = fan.id
              AND checkin.checked_in_at >= $5
              AND checkin.checked_in_at <= $4
        ) AS checkin_count
        WHERE fan.workspace_id = $1
          AND fan.status = 'active'
          AND fan.created_at <= $4
          AND COALESCE(
              (
                  SELECT max(token.consumed_at)
                  FROM fan_action_tokens AS token
                  WHERE token.workspace_id = fan.workspace_id
                    AND token.fan_id = fan.id
                    AND token.purpose = 'confirm'
                    AND token.consumed_at IS NOT NULL
              ),
              fan.created_at
          ) <= $4
          AND (
              $2 = 'all_active'
              OR EXISTS (
                  SELECT 1
                  FROM event_interests AS interest
                  WHERE interest.workspace_id = fan.workspace_id
                    AND interest.fan_id = fan.id
                    AND interest.event_id = $3
                    AND interest.created_at <= $4
              )
          )
        ORDER BY fan.id
        "#,
    )
    .bind(draw.workspace_id)
    .bind(&draw.eligibility_kind)
    .bind(draw.event_id)
    .bind(draw.closes_at)
    .bind(draw.opens_at)
    .fetch_all(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)
}

fn rank_candidates(
    candidates: Vec<CandidateRow>,
    draw: &DrawRow,
    seed: &[u8; 32],
) -> Result<Vec<RankedCandidate>, DrawWorkerError> {
    candidates
        .into_iter()
        .map(|candidate| {
            let qualified_referrals = i32::try_from(candidate.qualified_referrals)
                .map_err(|_| DrawWorkerError::Arithmetic)?;
            let available_bonus_entries = draw
                .max_entries
                .checked_sub(draw.base_entries)
                .ok_or(DrawWorkerError::Arithmetic)?;
            let referral_entries = qualified_referrals
                .checked_mul(draw.entries_per_referral)
                .ok_or(DrawWorkerError::Arithmetic)?
                .min(available_bonus_entries);
            let concert_checkins = i32::try_from(candidate.concert_checkins)
                .map_err(|_| DrawWorkerError::Arithmetic)?;
            let remaining_entries = available_bonus_entries
                .checked_sub(referral_entries)
                .ok_or(DrawWorkerError::Arithmetic)?;
            let checkin_entries = concert_checkins
                .checked_mul(draw.entries_per_checkin)
                .ok_or(DrawWorkerError::Arithmetic)?
                .min(remaining_entries);
            let entry_count = draw
                .base_entries
                .checked_add(referral_entries)
                .and_then(|value| value.checked_add(checkin_entries))
                .ok_or(DrawWorkerError::Arithmetic)?;
            let selection_score = weighted_score(seed, candidate.fan_id, entry_count)?;
            Ok(RankedCandidate {
                fan_id: candidate.fan_id,
                normalized_email: candidate.normalized_email,
                display_name: candidate.display_name,
                qualified_referrals,
                concert_checkins,
                checkin_entries,
                entry_count,
                selection_score,
            })
        })
        .collect()
}

#[allow(clippy::cast_precision_loss)]
fn weighted_score(seed: &[u8; 32], fan_id: Uuid, entry_count: i32) -> Result<f64, DrawWorkerError> {
    if entry_count <= 0 {
        return Err(DrawWorkerError::InvalidDraw);
    }
    let mut mac = HmacSha256::new_from_slice(seed).map_err(|_| DrawWorkerError::Entropy)?;
    mac.update(fan_id.as_bytes());
    let digest = mac.finalize().into_bytes();
    let bytes: [u8; 8] = digest
        .get(..8)
        .ok_or(DrawWorkerError::Entropy)?
        .try_into()
        .map_err(|_| DrawWorkerError::Entropy)?;
    let random = u64::from_be_bytes(bytes);
    // Use the 53 significant bits representable by f64. The half-step keeps
    // the value strictly inside (0, 1), so the exponential-race score can
    // never become zero or infinity at the database boundary.
    let mantissa = random >> 11;
    let unit = (mantissa as f64 + 0.5) / 9_007_199_254_740_992.0;
    Ok(-unit.ln() / f64::from(entry_count))
}

fn compare_candidates(left: &RankedCandidate, right: &RankedCandidate) -> Ordering {
    left.selection_score
        .total_cmp(&right.selection_score)
        .then_with(|| left.fan_id.cmp(&right.fan_id))
}

async fn persist_candidates(
    transaction: &mut Transaction<'_, Postgres>,
    draw: &DrawRow,
    run_id: Uuid,
    candidates: &[RankedCandidate],
) -> Result<(), DrawWorkerError> {
    if candidates.is_empty() {
        return Ok(());
    }

    let fan_ids: Vec<Uuid> = candidates
        .iter()
        .map(|candidate| candidate.fan_id)
        .collect();
    let qualified_referrals: Vec<i32> = candidates
        .iter()
        .map(|candidate| candidate.qualified_referrals)
        .collect();
    let concert_checkins: Vec<i32> = candidates
        .iter()
        .map(|candidate| candidate.concert_checkins)
        .collect();
    let checkin_entries: Vec<i32> = candidates
        .iter()
        .map(|candidate| candidate.checkin_entries)
        .collect();
    let entry_counts: Vec<i32> = candidates
        .iter()
        .map(|candidate| candidate.entry_count)
        .collect();
    let selection_scores: Vec<f64> = candidates
        .iter()
        .map(|candidate| candidate.selection_score)
        .collect();

    sqlx::query(
        r#"
        INSERT INTO reward_draw_candidates (
            workspace_id, draw_id, run_id, fan_id, qualified_referrals,
            concert_checkins, checkin_entries, entry_count, selection_score
        )
        SELECT $1, $2, $3, candidate.*
        FROM unnest(
            $4::uuid[],
            $5::integer[],
            $6::integer[],
            $7::integer[],
            $8::integer[],
            $9::double precision[]
        ) AS candidate(
            fan_id, qualified_referrals, concert_checkins,
            checkin_entries, entry_count, selection_score
        )
        "#,
    )
    .bind(draw.workspace_id)
    .bind(draw.id)
    .bind(run_id)
    .bind(fan_ids)
    .bind(qualified_referrals)
    .bind(concert_checkins)
    .bind(checkin_entries)
    .bind(entry_counts)
    .bind(selection_scores)
    .execute(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;
    Ok(())
}

async fn issue_admission_winners(
    transaction: &mut Transaction<'_, Postgres>,
    draw: &DrawRow,
    run_id: Uuid,
    candidates: &[RankedCandidate],
) -> Result<usize, DrawWorkerError> {
    let event_id = draw.event_id.ok_or(DrawWorkerError::InvalidDraw)?;
    let pool_id = draw.admission_pool_id.ok_or(DrawWorkerError::InvalidDraw)?;
    let pool = sqlx::query_as::<_, AdmissionPoolRow>(
        r#"
        SELECT id, capacity, issued_count, reserved_count
        FROM admission_pools
        WHERE workspace_id = $1 AND id = $2 AND event_id = $3 AND active
        FOR UPDATE
        "#,
    )
    .bind(draw.workspace_id)
    .bind(pool_id)
    .bind(event_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?
    .ok_or(DrawWorkerError::InvalidDraw)?;

    let available = pool
        .capacity
        .saturating_sub(pool.issued_count)
        .saturating_sub(pool.reserved_count);
    let target = usize::try_from(draw.winner_count.min(available).max(0))
        .map_err(|_| DrawWorkerError::Arithmetic)?;
    let mut selected = 0_usize;

    for candidate in candidates {
        if selected >= target {
            break;
        }
        if admission_already_exists(transaction, draw.workspace_id, pool.id, candidate.fan_id)
            .await?
        {
            continue;
        }

        let mut token_bytes = [0_u8; 32];
        fill_random(&mut token_bytes).map_err(|_| DrawWorkerError::Entropy)?;
        let claim_token = hex::encode(token_bytes);
        let reference_bytes = token_bytes.get(..6).ok_or(DrawWorkerError::Entropy)?;
        let public_reference = format!("VIRYA-{}", hex::encode(reference_bytes).to_uppercase());
        let pass_id = Uuid::now_v7();
        let claim_expires_at = OffsetDateTime::now_utc()
            .checked_add(time::Duration::hours(i64::from(draw.claim_expires_hours)))
            .ok_or(DrawWorkerError::Arithmetic)?;

        let inserted = sqlx::query(
            r#"
            INSERT INTO admission_passes (
                id, workspace_id, event_id, admission_pool_id, fan_id,
                issuance_method, public_reference, claim_token_hash,
                claim_expires_at, status
            )
            VALUES ($1, $2, $3, $4, $5, 'weighted_draw', $6, digest($7, 'sha256'), $8, 'issued')
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(pass_id)
        .bind(draw.workspace_id)
        .bind(event_id)
        .bind(pool.id)
        .bind(candidate.fan_id)
        .bind(&public_reference)
        .bind(&claim_token)
        .bind(claim_expires_at)
        .execute(&mut **transaction)
        .await
        .map_err(DrawWorkerError::sqlx)?;

        if inserted.rows_affected() == 0 {
            continue;
        }

        selected = selected.saturating_add(1);
        let winner_rank = i32::try_from(selected).map_err(|_| DrawWorkerError::Arithmetic)?;
        let _winner_id = record_winner(
            transaction,
            draw,
            run_id,
            candidate,
            winner_rank,
            Some(pass_id),
            None,
        )
        .await?;

        append_outbox(
            transaction,
            draw.workspace_id,
            "admission.pass.issued",
            &format!("draw:{}:fan:{}", draw.id, candidate.fan_id),
            json!({
                "pass_id": pass_id,
                "event_id": event_id,
                "admission_pool_id": pool.id,
                "fan_id": candidate.fan_id,
                "email": candidate.normalized_email,
                "display_name": candidate.display_name,
                "public_reference": public_reference,
                "claim_token": claim_token,
                "claim_expires_at": claim_expires_at,
                "issuance_method": "weighted_draw",
                "draw_id": draw.id,
                "draw_slug": draw.slug,
                "winner_rank": winner_rank,
                "entry_count": candidate.entry_count,
                "qualified_referrals": candidate.qualified_referrals,
                "concert_checkins": candidate.concert_checkins,
                "checkin_entries": candidate.checkin_entries,
            }),
        )
        .await?;
    }

    sqlx::query(
        "UPDATE admission_pools SET issued_count = issued_count + $3 WHERE workspace_id = $1 AND id = $2",
    )
    .bind(draw.workspace_id)
    .bind(pool.id)
    .bind(i32::try_from(selected).map_err(|_| DrawWorkerError::Arithmetic)?)
    .execute(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;

    Ok(selected)
}

async fn admission_already_exists(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    pool_id: Uuid,
    fan_id: Uuid,
) -> Result<bool, DrawWorkerError> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM admission_passes WHERE workspace_id = $1 AND admission_pool_id = $2 AND fan_id = $3)",
    )
    .bind(workspace_id)
    .bind(pool_id)
    .bind(fan_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)
}

async fn issue_physical_winners(
    transaction: &mut Transaction<'_, Postgres>,
    draw: &DrawRow,
    run_id: Uuid,
    candidates: &[RankedCandidate],
) -> Result<usize, DrawWorkerError> {
    let reward_rule_id = draw.reward_rule_id.ok_or(DrawWorkerError::InvalidDraw)?;
    let rule = sqlx::query_as::<_, PhysicalRuleRow>(
        r#"
        SELECT
            id,
            config->>'item_name' AS item_name,
            config->>'sku' AS sku,
            COALESCE((config->>'expires_days')::integer, 365) AS expires_days
        FROM reward_rules
        WHERE workspace_id = $1
          AND id = $2
          AND reward_type = 'physical_item'
          AND active
          AND btrim(COALESCE(config->>'item_name', '')) <> ''
          AND btrim(COALESCE(config->>'sku', '')) <> ''
          AND COALESCE((config->>'expires_days')::integer, 365) BETWEEN 1 AND 3650
        FOR SHARE
        "#,
    )
    .bind(draw.workspace_id)
    .bind(reward_rule_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?
    .ok_or(DrawWorkerError::InvalidDraw)?;

    let target = usize::try_from(draw.winner_count).map_err(|_| DrawWorkerError::Arithmetic)?;
    let mut selected = 0_usize;
    for candidate in candidates {
        if selected >= target {
            break;
        }
        let grant_id = Uuid::now_v7();
        let qualification_key = format!("weighted_draw:{}", draw.id);
        let inserted = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO reward_grants (
                id, workspace_id, fan_id, reward_rule_id, qualification_key,
                status, issued_at, expires_at
            )
            VALUES ($1, $2, $3, $4, $5, 'issued', now(), now() + ($6::bigint * interval '1 day'))
            ON CONFLICT (workspace_id, reward_rule_id, fan_id, qualification_key) DO NOTHING
            RETURNING id
            "#,
        )
        .bind(grant_id)
        .bind(draw.workspace_id)
        .bind(candidate.fan_id)
        .bind(rule.id)
        .bind(&qualification_key)
        .bind(rule.expires_days)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(DrawWorkerError::sqlx)?;

        let Some(grant_id) = inserted else {
            continue;
        };

        selected = selected.saturating_add(1);
        let winner_rank = i32::try_from(selected).map_err(|_| DrawWorkerError::Arithmetic)?;
        let winner_id = record_winner(
            transaction,
            draw,
            run_id,
            candidate,
            winner_rank,
            None,
            Some(grant_id),
        )
        .await?;

        sqlx::query(
            r#"
            INSERT INTO reward_draw_fulfillments (
                workspace_id, draw_id, winner_id, reward_grant_id,
                variant_id, quantity, status
            )
            SELECT
                allocation.workspace_id, allocation.draw_id, $3, $4,
                allocation.variant_id, allocation.units_per_winner, 'pending'
            FROM reward_draw_inventory_allocations AS allocation
            WHERE allocation.workspace_id = $1 AND allocation.draw_id = $2
            ON CONFLICT (workspace_id, winner_id) DO NOTHING
            "#,
        )
        .bind(draw.workspace_id)
        .bind(draw.id)
        .bind(winner_id)
        .bind(grant_id)
        .execute(&mut **transaction)
        .await
        .map_err(DrawWorkerError::sqlx)?;

        append_outbox(
            transaction,
            draw.workspace_id,
            "physical_reward.granted",
            &format!("draw:{}:fan:{}", draw.id, candidate.fan_id),
            json!({
                "workspace_id": draw.workspace_id,
                "reward_grant_id": grant_id,
                "reward_rule_id": rule.id,
                "fan_id": candidate.fan_id,
                "email": candidate.normalized_email,
                "display_name": candidate.display_name,
                "item_name": rule.item_name,
                "sku": rule.sku,
                "draw_id": draw.id,
                "draw_slug": draw.slug,
                "winner_rank": winner_rank,
                "entry_count": candidate.entry_count,
                "qualified_referrals": candidate.qualified_referrals,
                "concert_checkins": candidate.concert_checkins,
                "checkin_entries": candidate.checkin_entries,
            }),
        )
        .await?;
    }
    reconcile_physical_inventory_reservation(transaction, draw, selected).await?;
    Ok(selected)
}

async fn reconcile_physical_inventory_reservation(
    transaction: &mut Transaction<'_, Postgres>,
    draw: &DrawRow,
    selected: usize,
) -> Result<(), DrawWorkerError> {
    let selected = i32::try_from(selected).map_err(|_| DrawWorkerError::Arithmetic)?;
    let allocation = sqlx::query_as::<_, (Uuid, Uuid, i32)>(
        r#"
        SELECT reservation_id, variant_id, units_per_winner
        FROM reward_draw_inventory_allocations
        WHERE workspace_id = $1 AND draw_id = $2
        FOR UPDATE
        "#,
    )
    .bind(draw.workspace_id)
    .bind(draw.id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;
    let Some((reservation_id, variant_id, units_per_winner)) = allocation else {
        return Ok(());
    };
    let required = selected
        .checked_mul(units_per_winner)
        .ok_or(DrawWorkerError::Arithmetic)?;
    if required == 0 {
        sqlx::query(
            r#"
            DELETE FROM inventory_reservation_items
            WHERE workspace_id = $1 AND reservation_id = $2 AND variant_id = $3
            "#,
        )
        .bind(draw.workspace_id)
        .bind(reservation_id)
        .bind(variant_id)
        .execute(&mut **transaction)
        .await
        .map_err(DrawWorkerError::sqlx)?;
        sqlx::query(
            r#"
            UPDATE inventory_reservations
            SET status = 'released', released_at = now(),
                release_reason = 'draw completed without winners'
            WHERE workspace_id = $1 AND id = $2 AND status = 'active'
            "#,
        )
        .bind(draw.workspace_id)
        .bind(reservation_id)
        .execute(&mut **transaction)
        .await
        .map_err(DrawWorkerError::sqlx)?;
    } else {
        sqlx::query(
            r#"
            UPDATE inventory_reservation_items
            SET quantity = LEAST(quantity, $4)
            WHERE workspace_id = $1 AND reservation_id = $2 AND variant_id = $3
            "#,
        )
        .bind(draw.workspace_id)
        .bind(reservation_id)
        .bind(variant_id)
        .bind(required)
        .execute(&mut **transaction)
        .await
        .map_err(DrawWorkerError::sqlx)?;
    }
    Ok(())
}

async fn record_winner(
    transaction: &mut Transaction<'_, Postgres>,
    draw: &DrawRow,
    run_id: Uuid,
    candidate: &RankedCandidate,
    winner_rank: i32,
    admission_pass_id: Option<Uuid>,
    reward_grant_id: Option<Uuid>,
) -> Result<Uuid, DrawWorkerError> {
    let winner_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO reward_draw_winners (
            id, workspace_id, draw_id, run_id, fan_id, winner_rank,
            admission_pass_id, reward_grant_id
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
    )
    .bind(winner_id)
    .bind(draw.workspace_id)
    .bind(draw.id)
    .bind(run_id)
    .bind(candidate.fan_id)
    .bind(winner_rank)
    .bind(admission_pass_id)
    .bind(reward_grant_id)
    .execute(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;

    sqlx::query(
        r#"
        UPDATE reward_draw_candidates
        SET selected = true, winner_rank = $4
        WHERE workspace_id = $1 AND run_id = $2 AND fan_id = $3
        "#,
    )
    .bind(draw.workspace_id)
    .bind(run_id)
    .bind(candidate.fan_id)
    .bind(winner_rank)
    .execute(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;

    append_outbox(
        transaction,
        draw.workspace_id,
        "reward_draw.winner",
        &format!("draw:{}:winner:{}", draw.id, candidate.fan_id),
        json!({
            "draw_id": draw.id,
            "draw_slug": draw.slug,
            "draw_name": draw.name,
            "run_id": run_id,
            "fan_id": candidate.fan_id,
            "email": candidate.normalized_email,
            "display_name": candidate.display_name,
            "winner_rank": winner_rank,
            "entry_count": candidate.entry_count,
            "qualified_referrals": candidate.qualified_referrals,
            "concert_checkins": candidate.concert_checkins,
            "checkin_entries": candidate.checkin_entries,
            "prize_kind": draw.prize_kind,
            "admission_pass_id": admission_pass_id,
            "reward_grant_id": reward_grant_id,
        }),
    )
    .await?;
    Ok(winner_id)
}

async fn append_outbox(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    event_type: &str,
    request_id: &str,
    payload: serde_json::Value,
) -> Result<(), DrawWorkerError> {
    sqlx::query(
        "INSERT INTO outbox_events (workspace_id, event_type, event_version, payload, request_id) VALUES ($1, $2, 1, $3, $4)",
    )
    .bind(workspace_id)
    .bind(event_type)
    .bind(payload)
    .bind(request_id)
    .execute(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;
    Ok(())
}

async fn append_audit(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    action: &str,
    target_type: &str,
    target_id: Uuid,
    metadata: serde_json::Value,
) -> Result<(), DrawWorkerError> {
    sqlx::query(
        r#"
        INSERT INTO audit_events (
            workspace_id, actor_kind, action, target_type, target_id, metadata
        )
        VALUES ($1, 'system', $2, $3, $4, $5)
        "#,
    )
    .bind(workspace_id)
    .bind(action)
    .bind(target_type)
    .bind(target_id.to_string())
    .bind(metadata)
    .execute(&mut **transaction)
    .await
    .map_err(DrawWorkerError::sqlx)?;
    Ok(())
}

async fn record_failure(
    pool: &PgPool,
    draw_id: Uuid,
    error: &DrawWorkerError,
    config: &WeightedDrawWorkerConfig,
) -> Result<(), DrawWorkerError> {
    let message = truncate_error(&error.to_string());
    timeout(
        config.operation_timeout,
        sqlx::query(
            r#"
            UPDATE reward_draws
            SET attempts = attempts + 1,
                status = CASE WHEN attempts + 1 >= 10 THEN 'cancelled' ELSE 'scheduled' END,
                draw_at = CASE
                    WHEN attempts + 1 >= 10 THEN draw_at
                    ELSE now() + interval '5 minutes'
                END,
                last_error = $2
            WHERE id = $1 AND status IN ('scheduled', 'running')
            "#,
        )
        .bind(draw_id)
        .bind(message)
        .execute(pool),
    )
    .await
    .map_err(|_| DrawWorkerError::TimedOut)?
    .map_err(DrawWorkerError::sqlx)?;
    Ok(())
}

fn truncate_error(value: &str) -> String {
    value.chars().take(MAX_ERROR_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_is_deterministic_for_a_seed_and_fan() -> Result<(), Box<dyn std::error::Error>> {
        let seed = [7_u8; 32];
        let fan = Uuid::parse_str("018f7a7c-91f1-7d44-aec7-14fb1a20a999")?;
        assert_eq!(
            weighted_score(&seed, fan, 4)?,
            weighted_score(&seed, fan, 4)?
        );
        Ok(())
    }

    #[test]
    fn more_entries_improve_the_same_random_ticket() -> Result<(), Box<dyn std::error::Error>> {
        let seed = [11_u8; 32];
        let fan = Uuid::parse_str("018f7a7c-91f1-7d44-aec7-14fb1a20a999")?;
        assert!(weighted_score(&seed, fan, 10)? < weighted_score(&seed, fan, 1)?);
        Ok(())
    }

    #[test]
    fn checkins_fill_entry_cap_after_referrals() -> Result<(), Box<dyn std::error::Error>> {
        let draw = DrawRow {
            id: Uuid::from_u128(10),
            workspace_id: Uuid::from_u128(11),
            slug: "global-albums".to_owned(),
            name: "Global albums".to_owned(),
            prize_kind: "physical_item".to_owned(),
            eligibility_kind: "all_active".to_owned(),
            event_id: None,
            admission_pool_id: None,
            reward_rule_id: Some(Uuid::from_u128(12)),
            winner_count: 3,
            base_entries: 1,
            entries_per_referral: 2,
            entries_per_checkin: 1,
            max_entries: 6,
            claim_expires_hours: 168,
            closes_at: OffsetDateTime::UNIX_EPOCH + time::Duration::days(2),
            opens_at: OffsetDateTime::UNIX_EPOCH,
        };
        let ranked = rank_candidates(
            vec![CandidateRow {
                fan_id: Uuid::from_u128(13),
                normalized_email: "fan@example.test".to_owned(),
                display_name: None,
                qualified_referrals: 2,
                concert_checkins: 5,
            }],
            &draw,
            &[17_u8; 32],
        )?;
        assert_eq!(ranked[0].qualified_referrals, 2);
        assert_eq!(ranked[0].concert_checkins, 5);
        assert_eq!(ranked[0].checkin_entries, 1);
        assert_eq!(ranked[0].entry_count, 6);
        Ok(())
    }

    #[test]
    fn stable_tie_breaker_uses_fan_id() {
        let left = RankedCandidate {
            fan_id: Uuid::from_u128(1),
            normalized_email: "a@example.test".to_owned(),
            display_name: None,
            qualified_referrals: 0,
            concert_checkins: 0,
            checkin_entries: 0,
            entry_count: 1,
            selection_score: 0.5,
        };
        let right = RankedCandidate {
            fan_id: Uuid::from_u128(2),
            normalized_email: "b@example.test".to_owned(),
            display_name: None,
            qualified_referrals: 0,
            concert_checkins: 0,
            checkin_entries: 0,
            entry_count: 1,
            selection_score: 0.5,
        };
        assert_eq!(compare_candidates(&left, &right), Ordering::Less);
    }
}
