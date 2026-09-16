//! LLM call tuning — the autopilot's read-evaluate-write loop for worker
//! call parameters.
//!
//! Each cycle the worker asks this adapter to retune: load the recent call
//! tail the agent service wrote to `agent_service_llm_calls`, hand it to
//! `crowdrelay_brain::tune_llm::evaluate`, and upsert the decision into
//! `agent_service_llm_tuning` where the agent runner resolves it before each
//! call. All three steps live here so the rules module stays pure and the
//! caller stays one method call.
//!
//! The upsert runs on every cycle, including a decision of all defaults —
//! a cleared breaker or scale must actually clear, or the runner keeps
//! honoring a restriction the evidence no longer supports.
//!
//! No arithmetic happens in SQL beyond ordering: Postgres returns the tail
//! and the brain derives the decision, so the read model and the evidence
//! cannot drift apart.

use super::*;
use crowdrelay_brain::tune_llm::{self, LlmCallTail, LlmTuning};

/// How many recent calls the rules see. `tune_llm` reads a fixed-size tail —
/// wide enough for its windows, narrow enough that a cycle never loads the
/// whole history.
const TAIL_LIMIT: i64 = 50;

#[derive(Debug, FromRow)]
struct LlmCallRow {
    ok: bool,
    classified: Option<bool>,
    paid: bool,
    finish_reason: Option<String>,
    created_at: OffsetDateTime,
}

#[derive(Debug, FromRow)]
struct TuningRow {
    max_tokens_scale: Option<f64>,
    paid_breaker_until: Option<OffsetDateTime>,
}

impl PostgresAutopilotRepository {
    /// Re-evaluate the workspace's LLM call parameters from the recent call
    /// tail and persist the decision for the agent runner. Returns the
    /// decision so the caller can log what the brain concluded.
    ///
    /// A workspace with fewer calls than the rules' evidence floor gets a
    /// defaults row — the tuning relation holds "the brain looked and found
    /// nothing to change", which is different from "the brain never ran".
    pub async fn retune_llm(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<LlmTuning, RepositoryError> {
        let workspace = workspace_id.into_uuid();
        let rows = sqlx::query_as::<_, LlmCallRow>(
            r#"
            SELECT ok, classified, paid, finish_reason, created_at
            FROM agent_service_llm_calls
            WHERE workspace_id = $1
            ORDER BY created_at DESC, id DESC
            LIMIT $2
            "#,
        )
        .bind(workspace)
        .bind(TAIL_LIMIT)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;

        let calls: Vec<LlmCallTail<'_>> = rows
            .iter()
            .map(|row| LlmCallTail {
                ok: row.ok,
                classified: row.classified,
                paid: row.paid,
                finish_reason: row.finish_reason.as_deref(),
                created_at: row.created_at,
            })
            .collect();

        // The truncation rule raises from the scale already in effect, and
        // the breaker rule carries a live embargo forward — so the previous
        // decision is part of the input. Without it every firing would jump
        // straight from 1.0 and a quiet stretch would shorten a cooldown
        // that real failures earned.
        let previous = sqlx::query_as::<_, TuningRow>(
            r#"
            SELECT max_tokens_scale, paid_breaker_until
            FROM agent_service_llm_tuning
            WHERE workspace_id = $1
            "#,
        )
        .bind(workspace)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;
        let (previous_scale, previous_breaker) = previous
            .map(|row| (row.max_tokens_scale, row.paid_breaker_until))
            .unwrap_or((None, None));

        let tuning = tune_llm::evaluate(&calls, now, previous_scale, previous_breaker);

        sqlx::query(
            r#"
            INSERT INTO agent_service_llm_tuning
                (workspace_id, temperature, max_tokens_scale, paid_breaker_until, reason, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (workspace_id) DO UPDATE SET
                temperature        = EXCLUDED.temperature,
                max_tokens_scale   = EXCLUDED.max_tokens_scale,
                paid_breaker_until = EXCLUDED.paid_breaker_until,
                reason             = EXCLUDED.reason,
                updated_at         = EXCLUDED.updated_at
            "#,
        )
        .bind(workspace)
        .bind(tuning.temperature)
        .bind(tuning.max_tokens_scale)
        .bind(tuning.paid_breaker_until)
        .bind(&tuning.reason)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;

        Ok(tuning)
    }
}
