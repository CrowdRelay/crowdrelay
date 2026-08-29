//! Growth evidence repository — persistence for the unified evidence log.
//!
//! The brain records a `GrowthEvidence` row at dispatch time and loads
//! resolved evidence for learning. This module provides the SQL functions
//! to read and write the `viryaos_growth_evidence` table.
//!
//! See `crates/crowdrelay-brain/src/evidence.rs` for the domain types.

use crowdrelay_brain::{DispatchContext, GrowthEvidence, ReachChannel};
use crowdrelay_domain::WorkspaceId;
use time::OffsetDateTime;

use super::{PostgresAutopilotRepository, map_sqlx};
use crowdrelay_application::RepositoryError;

/// Records a growth evidence row at dispatch time. The outcome fields are
/// left NULL — they are filled in when measurements arrive.
pub(in crate::autopilot) async fn record_growth_evidence(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    evidence: &GrowthEvidence,
) -> Result<(), RepositoryError> {
    let pool = &repo.pool;
    let context_json = serde_json::to_value(&evidence.context).unwrap_or(serde_json::json!({}));
    sqlx::query(
        r#"
        INSERT INTO viryaos_growth_evidence (
            workspace_id, action_id, opportunity_id, timestamp,
            audience, recipient_id, channel, estimated_reach, actual_reach,
            treatment, propensity,
            observed_fans, observed_incremental_fans, durable_fans_30d,
            converted, converted_fan_id,
            predicted_fans, predicted_signal_installs, context
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)
        ON CONFLICT (workspace_id, action_id) DO NOTHING
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(evidence.action_id)
    .bind(&evidence.opportunity_id)
    .bind(evidence.timestamp)
    .bind(&evidence.audience)
    .bind(&evidence.recipient_id)
    .bind(evidence.channel.as_str())
    .bind(evidence.estimated_reach as i32)
    .bind(evidence.actual_reach.map(|v| v as i32))
    .bind(evidence.treatment.as_str())
    .bind(evidence.propensity)
    .bind(evidence.observed_fans)
    .bind(evidence.observed_incremental_fans)
    .bind(evidence.durable_fans_30d)
    .bind(evidence.converted)
    .bind(evidence.converted_fan_id)
    .bind(evidence.predicted_fans)
    .bind(evidence.predicted_signal_installs)
    .bind(&context_json)
    .execute(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

/// Loads resolved growth evidence for the brain's learning loop.
/// Returns only evidence rows that have a resolved outcome.
/// Ordered oldest-first so the brain can replay in chronological order.
pub(in crate::autopilot) async fn load_growth_evidence(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    since: Option<OffsetDateTime>,
) -> Result<Vec<GrowthEvidence>, RepositoryError> {
    let pool = &repo.pool;

    /// Evidence row from the database.
    #[derive(sqlx::FromRow)]
    struct EvidenceRow {
        action_id: uuid::Uuid,
        opportunity_id: Option<String>,
        timestamp: OffsetDateTime,
        audience: Option<String>,
        recipient_id: String,
        channel: String,
        estimated_reach: i32,
        actual_reach: Option<i32>,
        treatment: String,
        propensity: f64,
        observed_fans: Option<f64>,
        observed_incremental_fans: Option<f64>,
        durable_fans_30d: Option<f64>,
        converted: bool,
        converted_fan_id: Option<uuid::Uuid>,
        predicted_fans: f64,
        predicted_signal_installs: f64,
        context: serde_json::Value,
    }

    let rows: Vec<EvidenceRow> = sqlx::query_as(
        r#"
        SELECT action_id, opportunity_id, timestamp, audience, recipient_id,
               channel, estimated_reach, actual_reach, treatment, propensity,
               observed_fans, observed_incremental_fans, durable_fans_30d,
               converted, converted_fan_id, predicted_fans, predicted_signal_installs,
               context
        FROM viryaos_growth_evidence
        WHERE workspace_id = $1
          AND resolved_at IS NOT NULL
          AND ($2::timestamptz IS NULL OR timestamp > $2)
        ORDER BY timestamp ASC
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(since)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;

    let evidence: Vec<GrowthEvidence> = rows
        .into_iter()
        .map(|row| {
            let context: DispatchContext = serde_json::from_value(row.context).unwrap_or_default();
            let channel = ReachChannel::parse(&row.channel).unwrap_or(ReachChannel::Other);
            let treatment = match row.treatment.as_str() {
                "control" => crowdrelay_brain::TreatmentAssignment::Control,
                _ => crowdrelay_brain::TreatmentAssignment::Treatment,
            };
            GrowthEvidence {
                workspace_id: workspace_id.into_uuid(),
                opportunity_id: row.opportunity_id,
                action_id: row.action_id,
                timestamp: row.timestamp,
                audience: row.audience,
                recipient_id: row.recipient_id,
                channel,
                estimated_reach: row.estimated_reach.max(1) as u32,
                actual_reach: row.actual_reach.map(|v| v.max(0) as u32),
                treatment,
                propensity: row.propensity,
                observed_fans: row.observed_fans,
                observed_incremental_fans: row.observed_incremental_fans,
                durable_fans_30d: row.durable_fans_30d,
                converted: row.converted,
                converted_fan_id: row.converted_fan_id,
                predicted_fans: row.predicted_fans,
                predicted_signal_installs: row.predicted_signal_installs,
                context,
            }
        })
        .collect();
    Ok(evidence)
}

/// Saves a brain state checkpoint (serialized posterior state) for fast
/// startup. The brain loads the checkpoint on restart and applies only
/// delta evidence (evidence with timestamp > checkpoint).
pub(in crate::autopilot) async fn save_brain_state(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    module: &str,
    state: &serde_json::Value,
) -> Result<(), RepositoryError> {
    let pool = &repo.pool;
    sqlx::query(
        r#"
        INSERT INTO viryaos_brain_state (workspace_id, module, state, updated_at)
        VALUES ($1, $2, $3, now())
        ON CONFLICT (workspace_id, module)
        DO UPDATE SET state = $3, updated_at = now()
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(module)
    .bind(state)
    .execute(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

/// Loads a brain state checkpoint. Returns the serialized state and its
/// timestamp, or None if no checkpoint exists.
pub(in crate::autopilot) async fn load_brain_state(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    module: &str,
) -> Result<Option<(serde_json::Value, OffsetDateTime)>, RepositoryError> {
    let pool = &repo.pool;
    let row: Option<(serde_json::Value, OffsetDateTime)> = sqlx::query_as(
        r#"
        SELECT state, updated_at FROM viryaos_brain_state
        WHERE workspace_id = $1 AND module = $2
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(module)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(row)
}
