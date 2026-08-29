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
///
/// This also writes an immutable `action_dispatched` event to the
/// `viryaos_evidence_events` table and upserts the derived episode in
/// `viryaos_growth_episodes`.
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
            predicted_fans, predicted_signal_installs, context,
            episode_id, resolved_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21)
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
    .bind(&evidence.episode_id)
    .bind(evidence.resolved_at)
    .execute(pool)
    .await
    .map_err(map_sqlx)?;

    // Also write an immutable evidence event for the dispatch.
    let event = crowdrelay_brain::EvidenceEvent {
        workspace_id: workspace_id.into_uuid(),
        action_id: Some(evidence.action_id),
        opportunity_id: evidence.opportunity_id.clone(),
        episode_id: evidence.episode_id.clone(),
        event_type: crowdrelay_brain::EvidenceEventType::ActionDispatched,
        payload: serde_json::json!({
            "channel": evidence.channel.as_str(),
            "estimated_reach": evidence.estimated_reach,
            "treatment": evidence.treatment.as_str(),
            "propensity": evidence.propensity,
            "predicted_fans": evidence.predicted_fans,
            "predicted_signal_installs": evidence.predicted_signal_installs,
            "context": context_json,
        }),
        occurred_at: evidence.timestamp,
    };
    // Best-effort event write — don't fail the dispatch if the event log
    // write fails. The evidence table is the source of truth; the event
    // log is the audit trail.
    let _ = record_evidence_event(repo, workspace_id, &event).await;

    // Upsert the derived episode.
    let _ = upsert_growth_episode(repo, workspace_id, evidence).await;

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
        episode_id: Option<String>,
        resolved_at: Option<OffsetDateTime>,
    }

    let rows: Vec<EvidenceRow> = sqlx::query_as(
        r#"
        SELECT action_id, opportunity_id, timestamp, audience, recipient_id,
               channel, estimated_reach, actual_reach, treatment, propensity,
               observed_fans, observed_incremental_fans, durable_fans_30d,
               converted, converted_fan_id, predicted_fans, predicted_signal_installs,
               context, episode_id, resolved_at
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
                episode_id: row.episode_id,
                resolved_at: row.resolved_at,
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

/// Records an immutable evidence event to the `viryaos_evidence_events` table.
///
/// This is the append-only event log. Each call inserts a new row — no
/// updates, no deletes. The derived `viryaos_growth_episodes` table is
/// rebuilt from these events.
pub(in crate::autopilot) async fn record_evidence_event(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    event: &crowdrelay_brain::EvidenceEvent,
) -> Result<(), RepositoryError> {
    sqlx::query(
        r#"
        INSERT INTO viryaos_evidence_events
            (workspace_id, action_id, opportunity_id, episode_id,
             event_type, payload, occurred_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(event.action_id)
    .bind(&event.opportunity_id)
    .bind(&event.episode_id)
    .bind(event.event_type.as_str())
    .bind(&event.payload)
    .bind(event.occurred_at)
    .execute(&repo.pool)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

/// Upserts a growth episode — the derived aggregate from evidence events.
///
/// Called after recording an evidence event to keep the episode table in
/// sync. The episode is the brain's primary read path for evidence.
pub(in crate::autopilot) async fn upsert_growth_episode(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    evidence: &GrowthEvidence,
) -> Result<(), RepositoryError> {
    let context_json = serde_json::to_value(&evidence.context).unwrap_or(serde_json::json!({}));
    sqlx::query(
        r#"
        INSERT INTO viryaos_growth_episodes (
            workspace_id, action_id, opportunity_id, episode_id,
            channel, estimated_reach, treatment, propensity,
            predicted_fans, predicted_signal_installs, context,
            observed_fans, observed_incremental_fans, durable_fans_30d,
            actual_reach, converted, resolved_at, updated_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, now())
        ON CONFLICT (workspace_id, action_id) DO UPDATE SET
            observed_fans = EXCLUDED.observed_fans,
            observed_incremental_fans = EXCLUDED.observed_incremental_fans,
            durable_fans_30d = EXCLUDED.durable_fans_30d,
            actual_reach = EXCLUDED.actual_reach,
            converted = EXCLUDED.converted,
            resolved_at = EXCLUDED.resolved_at,
            updated_at = now()
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(evidence.action_id)
    .bind(&evidence.opportunity_id)
    .bind(&evidence.episode_id)
    .bind(evidence.channel.as_str())
    .bind(evidence.estimated_reach as i32)
    .bind(evidence.treatment.as_str())
    .bind(evidence.propensity)
    .bind(evidence.predicted_fans)
    .bind(evidence.predicted_signal_installs)
    .bind(&context_json)
    .bind(evidence.observed_fans)
    .bind(evidence.observed_incremental_fans)
    .bind(evidence.durable_fans_30d)
    .bind(evidence.actual_reach.map(|v| v as i32))
    .bind(evidence.converted)
    .bind(evidence.resolved_at)
    .execute(&repo.pool)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}
