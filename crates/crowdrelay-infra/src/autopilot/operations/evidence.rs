//! Growth evidence repository — persistence for the unified evidence log.
//!
//! The brain records a `GrowthEvidence` row at dispatch time and loads
//! resolved evidence for learning. This module provides the SQL functions
//! to read and write the `viryaos_growth_evidence` table.
//!
//! `viryaos_growth_evidence` is the primary read path for learning. The
//! `viryaos_growth_episodes` table (migration 0155) is a derived write-only
//! projection that is kept up to date as an audit trail; it is not read by
//! production learning code today despite the migration comment claiming
//! otherwise. `viryaos_evidence_events` is the immutable append-only event
//! log from which episodes can be rebuilt.
//!
//! See `crates/crowdrelay-brain/src/evidence.rs` for the domain types.

use crowdrelay_brain::{DispatchContext, GrowthEvidence, ReachChannel};
use crowdrelay_domain::WorkspaceId;
use time::OffsetDateTime;

use super::{PostgresAutopilotRepository, map_sqlx};
use crowdrelay_application::RepositoryError;

/// Records the best-effort audit trail for a growth evidence row:
/// the immutable `action_dispatched` event in `viryaos_evidence_events`
/// and the derived episode upsert in `viryaos_growth_episodes`.
///
/// This is NOT transactional with the evidence row — the evidence table
/// is the source of truth, and the event log / episode table are the
/// audit trail. Call this AFTER the transaction that writes the evidence
/// row commits. Failures are silently swallowed (best-effort).
pub(in crate::autopilot) async fn record_evidence_audit_trail(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    evidence: &GrowthEvidence,
) {
    let context_json = serde_json::to_value(&evidence.context).unwrap_or(serde_json::json!({}));
    let event = crowdrelay_brain::EvidenceEvent {
        workspace_id: workspace_id.into_uuid(),
        action_id: evidence.action_id,
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
            "strategy": evidence.strategy,
            "evidence_quality": evidence.evidence_quality.as_str(),
            "context": context_json,
        }),
        occurred_at: evidence.timestamp,
    };
    // Best-effort event write — don't fail the dispatch if the event log
    // write fails. The evidence table is the source of truth; the event
    // log is the audit trail.
    //
    // Best-effort, not silent. `let _ =` here would mean a write failing on
    // every dispatch looks identical to one succeeding, and the only way to
    // notice would be someone comparing row counts by hand. Three bugs found
    // in this repository had exactly that shape.
    if let Err(error) = record_evidence_event(repo, workspace_id, &event).await {
        tracing::warn!(
            %error,
            opportunity_id = ?evidence.opportunity_id,
            "evidence event log write failed; the evidence row itself was \
             written, so this is an audit-trail gap rather than lost data"
        );
    }

    // Upsert the derived episode.
    if let Err(error) = upsert_growth_episode(repo, workspace_id, evidence).await {
        tracing::warn!(
            %error,
            opportunity_id = ?evidence.opportunity_id,
            "growth episode upsert failed; the episode view will lag the \
             evidence it is derived from"
        );
    }
}

/// Records the growth evidence row within an existing transaction.
/// This is the critical write — the source of truth for the learning
/// loop. The event log and episode upsert are best-effort and remain
/// in the non-transactional `record_growth_evidence` wrapper.
///
/// Used by `record_experiment_assignment` to make control evidence
/// atomic with the assignment INSERT.
pub(in crate::autopilot) async fn record_growth_evidence_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    evidence: &GrowthEvidence,
) -> Result<(), RepositoryError> {
    let context_json = serde_json::to_value(&evidence.context).unwrap_or(serde_json::json!({}));
    sqlx::query(
        r#"
        INSERT INTO viryaos_growth_evidence (
            workspace_id, action_id, opportunity_id, timestamp,
            audience, target_key, creative_family, recipient_id, channel,
            estimated_reach, actual_reach,
            treatment, propensity,
            observed_fans, observed_incremental_fans, durable_fans_30d,
            converted, converted_fan_id,
            predicted_fans, predicted_signal_installs, context,
            strategy, evidence_quality,
            sample_size, contamination, measurement_delay_days,
            episode_id, resolved_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28)
        ON CONFLICT (workspace_id, action_id) DO NOTHING
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(evidence.action_id)
    .bind(&evidence.opportunity_id)
    .bind(evidence.timestamp)
    .bind(&evidence.audience)
    .bind(&evidence.target_key)
    .bind(evidence.creative_family.map(|f| f.as_str()))
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
    .bind(&evidence.strategy)
    .bind(evidence.evidence_quality.as_str())
    .bind(evidence.sample_size.map(|v| v as i32))
    .bind(evidence.contamination)
    .bind(evidence.measurement_delay_days.map(|v| v as i32))
    .bind(&evidence.episode_id)
    .bind(evidence.resolved_at)
    .execute(&mut **tx)
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

    // Diagnostic: check for legacy duplicate experiment assignments.
    // Migration 0201 added a partial UNIQUE INDEX on
    // (workspace_id, action_id) that prevents new duplicates, but
    // legacy rows predating the constraint may still exist. This is a
    // cheap existence check — EXISTS with LIMIT 1 short-circuits on
    // the first duplicate group found. The warning is emitted once
    // per invocation, not per duplicate row.
    //
    // This is diagnostic telemetry, not correctness logic:
    //   - query succeeds + duplicates exist → warn
    //   - query fails → loader continues normally (false = "could not
    //     establish duplicates exist", NOT "definitely no duplicates")
    let has_duplicates: bool = sqlx::query_scalar(
        r#"SELECT EXISTS(
            SELECT 1
            FROM viryaos_experiment_assignments
            WHERE workspace_id = $1 AND action_id IS NOT NULL
            GROUP BY workspace_id, action_id
            HAVING COUNT(*) > 1
            LIMIT 1
        )"#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_optional(pool)
    .await
    .map(Option::unwrap_or_default)
    .unwrap_or(false);
    if has_duplicates {
        tracing::warn!(
            workspace_id = %workspace_id.into_uuid(),
            "legacy duplicate experiment assignments detected — \
             migration 0201 prevents new ones but legacy data may need manual cleanup"
        );
    }

    /// Evidence row from the database.
    #[derive(sqlx::FromRow)]
    struct EvidenceRow {
        action_id: Option<uuid::Uuid>,
        opportunity_id: Option<String>,
        timestamp: OffsetDateTime,
        audience: Option<String>,
        target_key: Option<String>,
        creative_family: Option<String>,
        recipient_id: String,
        channel: String,
        estimated_reach: i32,
        actual_reach: Option<i32>,
        treatment: String,
        propensity: f64,
        execution_status: Option<String>,
        observed_fans: Option<f64>,
        observed_incremental_fans: Option<f64>,
        durable_fans_30d: Option<f64>,
        converted: bool,
        converted_fan_id: Option<uuid::Uuid>,
        predicted_fans: f64,
        predicted_signal_installs: f64,
        context: serde_json::Value,
        strategy: Option<String>,
        evidence_quality: String,
        sample_size: Option<i32>,
        contamination: Option<f64>,
        measurement_delay_days: Option<i32>,
        episode_id: Option<String>,
        resolved_at: Option<OffsetDateTime>,
    }

    let rows: Vec<EvidenceRow> = sqlx::query_as(
        r#"
        SELECT ge.action_id, ge.opportunity_id, ge.timestamp, ge.audience, ge.target_key,
               ge.creative_family, ge.recipient_id,
               ge.channel, ge.estimated_reach, ge.actual_reach, ge.treatment, ge.propensity,
               ea.execution_status,
               ge.observed_fans, ge.observed_incremental_fans, ge.durable_fans_30d,
               ge.converted, ge.converted_fan_id, ge.predicted_fans, ge.predicted_signal_installs,
               ge.context, ge.strategy, ge.evidence_quality,
               ge.sample_size, ge.contamination, ge.measurement_delay_days,
               ge.episode_id, ge.resolved_at
        FROM viryaos_growth_evidence ge
        -- LATERAL subquery: pick at most ONE assignment row per evidence
        -- row to prevent fan-out. A partial UNIQUE INDEX on
        -- (workspace_id, action_id) WHERE action_id IS NOT NULL
        -- (migration 0201) now enforces the 1:1 action-to-assignment
        -- invariant at the DB level. LATERAL + LIMIT 1 remains as a
        -- defensive safety net for legacy or externally imported data
        -- that might predate the constraint. Multiple assignments for
        -- one action are INVALID STATE, not a supported case — the
        -- LIMIT 1 does not make them acceptable.
        LEFT JOIN LATERAL (
            SELECT execution_status
            FROM viryaos_experiment_assignments ea
            WHERE ea.workspace_id = ge.workspace_id AND ea.action_id = ge.action_id
            LIMIT 1
        ) ea ON true
        WHERE ge.workspace_id = $1
          AND ge.resolved_at IS NOT NULL
          -- Exclude unresolved (unknown) executions from the causal
          -- learner. This is an explicit defense at the learning
          -- boundary — do NOT rely on the implicit chain
          -- (unknown → no measurement → resolved_at NULL → not loaded).
          --
          -- This filter only excludes `unknown` — the dangerous case
          -- where we cannot establish whether the treatment happened.
          -- The treatment/non-treatment distinction (executed vs failed
          -- vs control) is handled downstream by the causal layer's
          -- CausalEstimand::includes_in_treatment_effect(), not by this
          -- SQL filter. SQL provides eligible observations; the causal
          -- layer chooses the estimand.
          AND (ea.execution_status IS NULL OR ea.execution_status != 'unknown')
          AND ($2::timestamptz IS NULL OR ge.timestamp > $2)
        ORDER BY ge.timestamp ASC
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
            let execution_status = row.execution_status.as_deref().and_then(|s| {
                serde_json::from_value::<crowdrelay_brain::ExecutionStatus>(
                    serde_json::Value::String(s.to_owned()),
                )
                .ok()
            });
            GrowthEvidence {
                workspace_id: workspace_id.into_uuid(),
                opportunity_id: row.opportunity_id,
                action_id: row.action_id,
                timestamp: row.timestamp,
                audience: row.audience,
                target_key: row.target_key,
                creative_family: row
                    .creative_family
                    .as_deref()
                    .and_then(crowdrelay_domain::creative::CreativeFamily::parse),
                recipient_id: row.recipient_id,
                channel,
                estimated_reach: row.estimated_reach.max(1) as u32,
                actual_reach: row.actual_reach.map(|v| v.max(0) as u32),
                treatment,
                propensity: row.propensity,
                execution_status,
                observed_fans: row.observed_fans,
                observed_incremental_fans: row.observed_incremental_fans,
                durable_fans_30d: row.durable_fans_30d,
                converted: row.converted,
                converted_fan_id: row.converted_fan_id,
                predicted_fans: row.predicted_fans,
                predicted_signal_installs: row.predicted_signal_installs,
                context,
                strategy: row.strategy,
                evidence_quality: crowdrelay_brain::EvidenceQuality::parse(&row.evidence_quality)
                    .unwrap_or(crowdrelay_brain::EvidenceQuality::Observational),
                sample_size: row.sample_size.map(|v| v as u32),
                contamination: row.contamination,
                measurement_delay_days: row.measurement_delay_days.map(|v| v as u32),
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
             event_type, payload, occurred_at, trace_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7,
            (SELECT trace_id FROM viryaos_autopilot_actions WHERE id = $2)
        )
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

/// Records a credit allocation in the `viryaos_fan_credit_ledger` table.
///
/// CRITICAL INVARIANT: the raw observation in the evidence table is
/// immutable. This stores attributed credit in a SEPARATE table. The
/// learner consumes credited effects from the credit ledger, not raw
/// observations.
pub(in crate::autopilot) async fn record_credit_allocation(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    _outcome: &crowdrelay_brain::FanOutcome,
    result: &crowdrelay_brain::AttributionResult,
    measurement_id: Option<uuid::Uuid>,
    attribution_version: u32,
) -> Result<(), RepositoryError> {
    let pool = &repo.pool;
    let eligible_competitors = serde_json::to_string(
        &result
            .credits
            .iter()
            .map(|c| c.action_id.to_string())
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".to_string());

    for credit in &result.credits {
        sqlx::query(
            r#"
            INSERT INTO viryaos_fan_credit_ledger
                (workspace_id, action_id, credited_incremental_y14,
                 credited_incremental_y30, credit_weight,
                 attribution_confidence, attribution_method,
                 eligible_competitors, unattributed_residual,
                 evidence_quality, measurement_id, attribution_version,
                 is_causal_evidence)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
            ON CONFLICT (measurement_id, attribution_version, action_id)
                WHERE measurement_id IS NOT NULL
            DO NOTHING
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(credit.action_id)
        .bind(credit.credited_incremental_y14)
        .bind(credit.credited_incremental_y30)
        .bind(credit.credit_weight)
        .bind(credit.attribution_confidence)
        .bind(result.method.as_str())
        .bind(&eligible_competitors)
        .bind(result.unattributed)
        .bind("modeled_attribution") // ATTRIBUTION ≠ CAUSAL EVIDENCE
        .bind(measurement_id)
        .bind(attribution_version as i32)
        .bind(credit.is_causal_evidence)
        .execute(pool)
        .await
        .map_err(map_sqlx)?;
    }
    // Verify the attribution invariant: sum(credits) + unattributed ≈ observed.
    let total_credited: f64 = result
        .credits
        .iter()
        .map(|c| c.credited_incremental_y14)
        .sum();
    let _ = total_credited + result.unattributed; // should ≈ outcome.observed_incremental_fans
    Ok(())
}

/// Counts measurements that have not resolved yet. Used by the WAIT
/// candidate's value-of-information computation: waiting is worth something
/// precisely when outcomes are still coming.
///
/// Counted from the measurement queue rather than from
/// `viryaos_growth_evidence.resolved_at`. Four measurements resolve against
/// one evidence row and the first to land stamps `resolved_at`, so a
/// dispatch with three outcomes still outstanding read as fully observed and
/// dropped out of the count. VOI was undercounted by roughly the ratio of
/// measurements to dispatches — which is to say WAIT almost never had a
/// reason to win.
pub(in crate::autopilot) async fn count_pending_measurements(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
) -> Result<u32, RepositoryError> {
    repo.bounded(async {
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM viryaos_autopilot_measurements
            WHERE workspace_id = $1
              AND status = 'pending'
            "#,
        )
        .bind(workspace_id.into_uuid())
        .fetch_one(&repo.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(u32::try_from(count).unwrap_or(u32::MAX))
    })
    .await
}

/// Discovers competing actions for attribution — all treatment evidence
/// rows in the same workspace whose dispatch window overlaps with the
/// outcome's measurement window. Returns `ActionExposure` vectors for
/// the `CreditAllocator`.
///
/// Phase 1: uses simple heuristics for temporal proximity and audience
/// match. Phase 2 can use richer features (reach overlap, topic
/// similarity, etc.).
pub(in crate::autopilot) async fn discover_competing_actions(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    outcome_action_id: uuid::Uuid,
    window_start: time::OffsetDateTime,
    window_end: time::OffsetDateTime,
) -> Result<Vec<crowdrelay_brain::ActionExposure>, RepositoryError> {
    #[derive(sqlx::FromRow)]
    struct CompetingRow {
        action_id: uuid::Uuid,
        timestamp: time::OffsetDateTime,
        evidence_quality: String,
        predicted_fans: f64,
        recipient_id: Option<String>,
        opportunity_id: Option<String>,
    }
    let rows: Vec<CompetingRow> = sqlx::query_as(
        r#"
        SELECT
            action_id,
            timestamp,
            evidence_quality,
            predicted_fans,
            recipient_id,
            opportunity_id
        FROM viryaos_growth_evidence
        WHERE workspace_id = $1
          AND action_id IS NOT NULL
          AND action_id != $2
          AND timestamp >= $3 - INTERVAL '14 days'
          AND timestamp <= $4
          AND treatment = 'treatment'
        ORDER BY timestamp ASC
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(outcome_action_id)
    .bind(window_start)
    .bind(window_end)
    .fetch_all(&repo.pool)
    .await
    .map_err(map_sqlx)?;
    let window_duration = (window_end - window_start).as_seconds_f64().max(1.0);
    let exposures = rows
        .into_iter()
        .map(|row| {
            let dispatch_offset = (row.timestamp - window_start).as_seconds_f64().abs();
            let temporal_proximity = (1.0 - dispatch_offset / window_duration).max(0.0);
            let audience_key = row.recipient_id.unwrap_or_default();
            let evidence_quality = crowdrelay_brain::EvidenceQuality::parse(&row.evidence_quality)
                .unwrap_or(crowdrelay_brain::EvidenceQuality::Observational);
            crowdrelay_brain::ActionExposure {
                action_id: row.action_id,
                template_id: row
                    .opportunity_id
                    .as_deref()
                    .and_then(|s| s.split(':').next())
                    .unwrap_or("unknown")
                    .to_owned(),
                audience_key,
                exposure_delivered: true,
                temporal_proximity,
                // Not modelled. Whether this action's audience is the one the
                // fan actually came from needs the fan's provenance joined in,
                // and nothing here has it.
                //
                // The value was 0.5, which reads as "half a match" and is not:
                // every exposure carried it, so it cancelled in the weight
                // normalisation and meant nothing at all. A constant that
                // looks like a measurement is worse than a neutral one, so it
                // is 1.0 and says what it is.
                audience_match: 1.0,
                // Confidence is the evidence quality, which was already
                // loaded on this row and then ignored in favour of a flat
                // 0.7. The credit allocator bounds total attribution mass by
                // the mean confidence — that is the mechanism that preserves
                // a genuine "we don't know" residual — so a constant made the
                // residual exactly 30% by fiat whatever the evidence said,
                // and let observational evidence claim 70% of every fan it
                // had no causal claim on. Randomised holdout earns 1.0,
                // observational 0.1.
                attribution_confidence: evidence_quality.weight(),
                treatment_effect_prior: row.predicted_fans,
                evidence_quality,
            }
        })
        .collect();
    Ok(exposures)
}
