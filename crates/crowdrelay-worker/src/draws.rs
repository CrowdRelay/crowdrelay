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
    eligibility_ref: Option<String>,
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

async fn lock_due_draw(
    transaction: &mut Transaction<'_, Postgres>,
) -> Result<Option<DrawRow>, DrawWorkerError> {
    let row = sqlx::query_as::<_, DrawRow>(
        r#"
        SELECT
            id, workspace_id, slug, name, prize_kind, eligibility_kind, eligibility_ref,
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

include!("draws/execution.rs");
fn validate_draw(draw: &DrawRow) -> Result<(), DrawWorkerError> {
    if draw.winner_count <= 0
        || draw.base_entries <= 0
        || draw.entries_per_referral < 0
        || draw.entries_per_checkin < 0
        || draw.max_entries < draw.base_entries
        || draw.claim_expires_hours <= 0
        || !matches!(
            draw.eligibility_kind.as_str(),
            "all_active" | "event_interest" | "synesthesia_completion"
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
    if draw.eligibility_kind == "synesthesia_completion"
        && (draw.eligibility_ref.as_deref().is_none_or(str::is_empty)
            || draw.winner_count != 5
            || draw.base_entries != 1
            || draw.entries_per_referral != 0
            || draw.entries_per_checkin != 0
            || draw.max_entries != 1)
    {
        return Err(DrawWorkerError::InvalidDraw);
    }
    Ok(())
}

include!("draws/candidates_and_rewards.rs");
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
            eligibility_ref: None,
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
