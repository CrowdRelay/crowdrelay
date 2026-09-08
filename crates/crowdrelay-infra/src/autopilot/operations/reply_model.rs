//! Reply probability model repository — persistence and loading for the
//! hierarchical Beta-Bernoulli P(positive reply) model.
//!
//! The model is stored as a serialized jsonb blob in `viryaos_brain_state`
//! with `module = 'reply_probability'`, following the same pattern as the
//! causal model checkpoint. On load, the checkpoint is deserialized and
//! updated from outreach interaction outcomes observed since the checkpoint
//! timestamp.
//!
//! The model is an additive, reversible advisory signal. An empty model
//! (cold start or deserialization failure) returns the global prior for
//! every prediction, so the system falls back to `relevance_basis_points`
//! ranking. See `crates/crowdrelay-brain/src/reply_model.rs`.

use crowdrelay_brain::{ReplyOutcome, ReplyProbabilityModel, disposition_to_label};
use crowdrelay_domain::WorkspaceId;
use sqlx::Row;
use time::OffsetDateTime;

use super::{PostgresAutopilotRepository, map_sqlx};
use crate::autopilot::RepositoryError;

/// The brain-state module key for the reply probability model.
const MODULE: &str = "reply_probability";

/// Loads the reply probability model from its brain-state checkpoint,
/// then updates it from outreach interaction outcomes observed since the
/// checkpoint timestamp. If no checkpoint exists or deserialization fails,
/// returns a fresh model (cold start) and updates it from all historical
/// outcomes.
pub(in crate::autopilot) async fn load_reply_model(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<ReplyProbabilityModel, RepositoryError> {
    let checkpoint = super::evidence::load_brain_state(repo, workspace_id, MODULE).await?;
    let (mut model, checkpoint_time) = match checkpoint {
        Some((state_json, ts)) => {
            match serde_json::from_value::<ReplyProbabilityModel>(state_json) {
                Ok(m) => (m, Some(ts)),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "reply model checkpoint deserialization failed, starting cold"
                    );
                    (ReplyProbabilityModel::new(), None)
                }
            }
        }
        None => (ReplyProbabilityModel::new(), None),
    };
    // Load outreach interaction outcomes since the checkpoint (or all
    // history if cold start) and update the model.
    let outcomes = load_outreach_reply_outcomes(repo, workspace_id, checkpoint_time).await?;
    if !outcomes.is_empty() {
        model.update_all(&outcomes);
        tracing::debug!(
            outcomes = outcomes.len(),
            cold_start = checkpoint_time.is_none(),
            "updated reply model from outreach outcomes"
        );
    }
    Ok(model)
}

/// Saves the reply probability model checkpoint for fast startup on the
/// next cycle. Best-effort: a failed checkpoint just means the next cycle
/// rebuilds from full history.
pub(in crate::autopilot) async fn save_reply_model(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    model: &ReplyProbabilityModel,
) -> Result<(), RepositoryError> {
    let state = serde_json::to_value(model).map_err(|e| {
        tracing::warn!(error = %e, "failed to serialize reply model checkpoint");
        RepositoryError::Unexpected
    })?;
    super::evidence::save_brain_state(repo, workspace_id, MODULE, &state).await
}

/// Loads outreach interaction outcomes for the reply model.
///
/// Each outbound outreach interaction that has a corresponding inbound reply
/// produces a `ReplyOutcome`. The `observed_positive` field is `true` when
/// the reply disposition is `positive`, `false` otherwise (declined,
/// do_not_contact, received, or no reply observed).
///
/// When `since` is `None`, all historical outcomes are loaded (cold start).
/// When `since` is `Some(ts)`, only outcomes after `ts` are loaded (delta
/// replay from checkpoint).
pub(in crate::autopilot) async fn load_outreach_reply_outcomes(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    since: Option<OffsetDateTime>,
) -> Result<Vec<ReplyOutcome>, RepositoryError> {
    let pool = &repo.pool;
    // Join outbound interactions to their target's kind and to any inbound
    // reply on the same target. An outbound interaction with no matching
    // inbound reply is a "no reply" outcome (observed_positive = false).
    // An outbound interaction with a matching inbound reply uses the
    // reply's disposition.
    //
    // The observation window for "no reply" is 30 days after the outbound
    // interaction — if no reply arrived within 30 days, the pitch is
    // treated as a negative outcome. Replies that arrived later are still
    // counted, but only if they arrived before the query runs.
    //
    // Delta replay filters by the observation timestamp (when the outcome
    // became knowable), NOT by the outbound timestamp. A no-reply outcome
    // becomes knowable at `outbound.occurred_at + 30 days`; filtering by
    // `outbound.occurred_at > checkpoint` would skip no-replies whose
    // outbound preceded the checkpoint but whose 30-day window closed
    // after it, systematically dropping negative outcomes and biasing the
    // model optimistic.
    let rows = sqlx::query(
        r#"
        WITH observed AS (
            SELECT
                target.target_kind,
                target.id::text AS target_id,
                COALESCE(reply.disposition, 'none') AS disposition,
                -- When the outcome became knowable: the reply timestamp if
                -- one arrived, otherwise the 30-day no-reply deadline.
                COALESCE(
                    reply.occurred_at,
                    outbound.occurred_at + INTERVAL '30 days'
                ) AS observed_at
            FROM viryaos_outreach_interactions AS outbound
            JOIN viryaos_outreach_targets AS target
              ON target.workspace_id = outbound.workspace_id
             AND target.id = outbound.target_id
            LEFT JOIN LATERAL (
                SELECT interaction.disposition, interaction.occurred_at
                FROM viryaos_outreach_interactions AS interaction
                WHERE interaction.workspace_id = outbound.workspace_id
                  AND interaction.target_id = outbound.target_id
                  AND interaction.direction = 'inbound'
                  AND interaction.occurred_at >= outbound.occurred_at
                ORDER BY interaction.occurred_at ASC
                LIMIT 1
            ) AS reply ON true
            WHERE outbound.workspace_id = $1
              AND outbound.direction = 'outbound'
              -- Only count outcomes where the observation window has closed:
              -- either a reply arrived, or 30 days have passed since the
              -- outbound.
              AND (
                  reply.occurred_at IS NOT NULL
                  OR outbound.occurred_at < now() - INTERVAL '30 days'
              )
        )
        SELECT target_kind, target_id, disposition
        FROM observed
        WHERE $2::timestamptz IS NULL OR observed_at > $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(since)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    let mut outcomes = Vec::with_capacity(rows.len());
    for row in rows {
        let target_kind: String = row
            .try_get("target_kind")
            .map_err(|_| RepositoryError::Unexpected)?;
        let target_id: String = row
            .try_get("target_id")
            .map_err(|_| RepositoryError::Unexpected)?;
        let disposition: String = row
            .try_get("disposition")
            .map_err(|_| RepositoryError::Unexpected)?;
        let observed_positive = disposition_to_label(&disposition);
        outcomes.push(ReplyOutcome {
            target_kind,
            target_id,
            observed_positive,
            disposition,
        });
    }
    Ok(outcomes)
}
