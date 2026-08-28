//! Agent outcome ingestion worker.
//!
//! Polls the `agent_outcomes` handoff table for rows written by the
//! `crowdrelay-agents` TypeScript service, validates each payload against the
//! versioned Rust mirror of the zod schemas, and maps the outcome into
//! autopilot decision (+ action, for `require_approval` kinds) rows.
//!
//! Ownership: the agents service is the ONLY writer of `agent_outcomes`; this
//! worker is the only reader/mapper. `agent_fan_segments` and
//! `agent_outreach_targets` are written here too — single-writer per table.
//!
//! Idempotency: `agent_outcomes.idempotency_key` is unique per
//! (workspace_id, key), and the autopilot decision_key mirrors it, so worker
//! retries and task re-runs can never double-create decisions.

use std::time::Duration;

use crowdrelay_application::agent_outcomes::{OutcomeKind, ValidatedOutcome, validate};
use crowdrelay_domain::WorkspaceId;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use thiserror::Error;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

const BATCH_LIMIT: i64 = 32;

#[derive(Debug, Error)]
enum AgentOutcomeError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("validation error: {0}")]
    Validation(#[from] crowdrelay_application::agent_outcomes::OutcomeValidationError),
}

#[derive(Clone, Debug)]
pub struct AgentOutcomeWorker {
    pool: PgPool,
    workspace_id: WorkspaceId,
    poll_interval: Duration,
    operation_timeout: Duration,
}

impl AgentOutcomeWorker {
    #[must_use]
    pub fn new(
        pool: PgPool,
        workspace_id: WorkspaceId,
        poll_interval: Duration,
        operation_timeout: Duration,
    ) -> Self {
        Self {
            pool,
            workspace_id,
            poll_interval,
            operation_timeout,
        }
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticks = interval(self.poll_interval);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                _ = ticks.tick() => {
                    match timeout(self.operation_timeout, self.run_once()).await {
                        Ok(Ok(processed)) if processed > 0 => {
                            tracing::info!(processed, "agent outcome worker processed batch");
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => tracing::warn!(error = %error, "agent outcome worker cycle failed"),
                        Err(_) => tracing::warn!("agent outcome worker cycle timed out"),
                    }
                }
            }
        }
    }

    async fn run_once(&self) -> Result<usize, AgentOutcomeError> {
        let mut total = 0;
        loop {
            let processed = self.process_batch().await?;
            if processed == 0 {
                break;
            }
            total += processed;
        }
        Ok(total)
    }

    /// Claims one batch of pending outcomes (FOR UPDATE SKIP LOCKED), validates
    /// each, maps to autopilot rows, and marks the outcome processed or
    /// rejected. Each outcome is its own transaction so one bad payload cannot
    /// roll back a whole batch.
    async fn process_batch(&self) -> Result<usize, AgentOutcomeError> {
        let rows = sqlx::query_as::<_, OutcomeRow>(
            r#"
            UPDATE agent_outcomes
            SET status = 'processing'
            WHERE id IN (
                SELECT id FROM agent_outcomes
                WHERE workspace_id = $1 AND status = 'pending'
                ORDER BY created_at
                LIMIT $2
                FOR UPDATE SKIP LOCKED
            )
            RETURNING id, workspace_id, task_id, result_id, kind, schema_version,
                      payload, confidence_basis_points, idempotency_key
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(BATCH_LIMIT)
        .fetch_all(&self.pool)
        .await?;

        let mut processed = 0;
        for row in rows {
            let outcome = match validate(
                row.id,
                row.workspace_id,
                row.task_id,
                row.result_id,
                &row.kind,
                row.schema_version,
                &row.payload,
                row.confidence_basis_points,
                row.idempotency_key.clone(),
            ) {
                Ok(outcome) => outcome,
                Err(error) => {
                    tracing::warn!(
                        outcome_id = %row.id,
                        error = %error,
                        "rejecting agent outcome"
                    );
                    self.reject_outcome(row.id, &error.to_string()).await?;
                    continue;
                }
            };

            match self.map_outcome(&outcome).await {
                Ok((decision_id, action_id)) => {
                    self.mark_processed(outcome.id, decision_id, action_id)
                        .await?;
                    processed += 1;
                }
                Err(error) => {
                    tracing::warn!(
                        outcome_id = %outcome.id,
                        error = %error,
                        "failed to map agent outcome"
                    );
                    self.reject_outcome(outcome.id, &error.to_string()).await?;
                }
            }
        }
        Ok(processed)
    }

    /// Maps a validated outcome into autopilot decision (+ action) rows and
    /// any side tables (fan_segments, outreach_targets) in one transaction.
    async fn map_outcome(
        &self,
        outcome: &ValidatedOutcome,
    ) -> Result<(Option<Uuid>, Option<Uuid>), AgentOutcomeError> {
        let mut tx = self.pool.begin().await?;
        let decision_id = Uuid::now_v7();
        let input_snapshot = json!({
            "task_id": outcome.task_id,
            "result_id": outcome.result_id,
            "schema_version": outcome.schema_version,
            "payload": outcome.payload,
        });

        // Insert the decision row. decision_key mirrors the outcome's
        // idempotency_key so a worker retry is a no-op.
        let inserted_decision = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO viryaos_autopilot_decisions (
                id, workspace_id, decision_key, context, subject_kind, subject_id,
                decision_kind, confidence_basis_points, disposition, reason,
                input_snapshot, policy_snapshot, recommendation
            )
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
            ON CONFLICT (workspace_id, decision_key) DO NOTHING
            RETURNING id
            "#,
        )
        .bind(decision_id)
        .bind(outcome.workspace_id)
        .bind(&outcome.idempotency_key)
        .bind(outcome.kind.autopilot_context())
        .bind("agent_task")
        .bind(outcome.task_id)
        .bind(outcome.kind.decision_kind())
        .bind(outcome.confidence_basis_points)
        .bind(outcome.kind.disposition())
        .bind(
            // The autopilot_decisions.reason column has a CHECK constraint
            // (non-empty, <=240 chars). The LLM rationale can be longer, so
            // truncate to fit. The full rationale is preserved in
            // input_snapshot.payload.rationale.
            outcome
                .payload
                .rationale
                .get(..outcome.payload.rationale.ceil_char_boundary(240))
                .unwrap_or(&outcome.payload.rationale),
        )
        .bind(&input_snapshot)
        .bind(json!({ "source": "agent_outcome", "schema_version": outcome.schema_version }))
        .bind(json!({}))
        .fetch_optional(&mut *tx)
        .await?;

        // Side tables per kind. Single-writer: only this worker inserts here.
        match outcome.kind {
            OutcomeKind::AudienceSegments => {
                if let Some(item) = &outcome.payload.item {
                    self.insert_fan_segment(&mut tx, outcome, item).await?;
                }
            }
            OutcomeKind::OutreachTargets => {
                if let Some(item) = &outcome.payload.item {
                    self.insert_outreach_target(&mut tx, outcome, item).await?;
                }
            }
            _ => {}
        }

        // Action row only for require_approval kinds.
        let action_id = if outcome.kind.disposition() == "require_approval" {
            let action_id = Uuid::now_v7();
            let payload = match outcome.kind {
                OutcomeKind::PressPitch | OutcomeKind::SocialPost => json!({
                    "kind": "request_agent_content",
                    "template_id": outcome
                        .payload
                        .item
                        .as_ref()
                        .and_then(|i| i.get("template_id"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                    "task_id": outcome.task_id,
                    "draft": outcome.payload.item.clone().unwrap_or(Value::Null),
                }),
                OutcomeKind::OutreachTargets => json!({
                    "kind": "request_agent_content",
                    "template_id": "outreach-targets",
                    "task_id": outcome.task_id,
                    "draft": outcome.payload.item.clone().unwrap_or(Value::Null),
                }),
                _ => Value::Null,
            };
            let action_kind = match outcome.kind {
                OutcomeKind::PressPitch | OutcomeKind::SocialPost => "agent.content.request",
                OutcomeKind::OutreachTargets => "outreach.request",
                _ => "agent.content.request",
            };
            sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO viryaos_autopilot_actions (
                    id, workspace_id, decision_id, context, action_kind,
                    subject_kind, subject_id, idempotency_key, payload, status,
                    action_class, approved_at, approved_by, approval_expires_at
                )
                VALUES (
                    $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,
                    NULL, NULL,
                    now() + INTERVAL '72 hours'
                )
                ON CONFLICT DO NOTHING
                RETURNING id
                "#,
            )
            .bind(action_id)
            .bind(outcome.workspace_id)
            .bind(decision_id)
            .bind(outcome.kind.autopilot_context())
            .bind(action_kind)
            .bind("agent_task")
            .bind(outcome.task_id)
            .bind(&outcome.idempotency_key)
            .bind(&payload)
            .bind("awaiting_approval")
            .bind("first_party_reversible")
            .fetch_optional(&mut *tx)
            .await?
        } else {
            None
        };

        tx.commit().await?;
        Ok((inserted_decision, action_id))
    }

    /// Inserts an `agent_fan_segments` row from an audience_segments item.
    /// `UNIQUE (workspace_id, name)` makes a re-run a no-op.
    async fn insert_fan_segment(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        outcome: &ValidatedOutcome,
        item: &Value,
    ) -> Result<(), AgentOutcomeError> {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Unnamed segment");
        let description = item
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let size_estimate = item
            .get("size_estimate")
            .and_then(Value::as_i64)
            .map(i32::try_from)
            .and_then(Result::ok);
        let criteria = item.get("criteria").cloned().unwrap_or(json!({}));
        sqlx::query(
            r#"
            INSERT INTO agent_fan_segments
                (workspace_id, name, description, size_estimate, criteria, source_task_id)
            VALUES ($1,$2,$3,$4,$5,$6)
            ON CONFLICT (workspace_id, name) DO NOTHING
            "#,
        )
        .bind(outcome.workspace_id)
        .bind(name)
        .bind(description)
        .bind(size_estimate)
        .bind(&criteria)
        .bind(outcome.task_id)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Inserts an `agent_outreach_targets` staging row. `verified=false` by
    /// default — operator verification is the approval that flips it.
    async fn insert_outreach_target(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        outcome: &ValidatedOutcome,
        item: &Value,
    ) -> Result<(), AgentOutcomeError> {
        let target_kind = item
            .get("target_kind")
            .and_then(Value::as_str)
            .unwrap_or("press");
        let display_name = item
            .get("display_name")
            .and_then(Value::as_str)
            .unwrap_or("Unnamed target");
        let contact_email = item.get("contact_email").and_then(Value::as_str);
        let contact_domain = item.get("contact_domain").and_then(Value::as_str);
        let why_fit = item.get("why_fit").and_then(Value::as_str).unwrap_or("");
        let evidence = item.get("evidence_urls").cloned().unwrap_or(json!([]));
        sqlx::query(
            r#"
            INSERT INTO agent_outreach_targets
                (workspace_id, target_kind, display_name, contact_email, contact_domain,
                 why_fit, evidence, source_task_id)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
            "#,
        )
        .bind(outcome.workspace_id)
        .bind(target_kind)
        .bind(display_name)
        .bind(contact_email)
        .bind(contact_domain)
        .bind(why_fit)
        .bind(&evidence)
        .bind(outcome.task_id)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn mark_processed(
        &self,
        outcome_id: Uuid,
        decision_id: Option<Uuid>,
        action_id: Option<Uuid>,
    ) -> Result<(), AgentOutcomeError> {
        sqlx::query(
            r#"
            UPDATE agent_outcomes
            SET status = 'processed',
                processed_decision_id = $2,
                processed_action_id = $3,
                processed_at = now()
            WHERE id = $1
            "#,
        )
        .bind(outcome_id)
        .bind(decision_id)
        .bind(action_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn reject_outcome(
        &self,
        outcome_id: Uuid,
        reason: &str,
    ) -> Result<(), AgentOutcomeError> {
        sqlx::query(
            r#"
            UPDATE agent_outcomes
            SET status = 'rejected', rejection_reason = $2
            WHERE id = $1
            "#,
        )
        .bind(outcome_id)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
struct OutcomeRow {
    id: Uuid,
    workspace_id: Uuid,
    task_id: Uuid,
    result_id: Uuid,
    kind: String,
    schema_version: i32,
    payload: Value,
    confidence_basis_points: i32,
    idempotency_key: String,
}
