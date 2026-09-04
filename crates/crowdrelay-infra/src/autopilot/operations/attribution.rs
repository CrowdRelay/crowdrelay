//! Attribution worker logic — processes pending attribution requests
//! from the outbox and writes credited entries to the credit ledger.
//!
//! When a measurement completes, an attribution request is enqueued in
//! `viryaos_attribution_requests`. This module provides the logic to
//! claim pending requests, discover competing actions, run the
//! `ProportionalCreditAllocator`, and write the result to
//! `viryaos_fan_credit_ledger`. The write is idempotent on
//! (measurement_id, attribution_version).

use crowdrelay_brain::{CreditAllocator, FanOutcome, ProportionalCreditAllocator};
use crowdrelay_domain::WorkspaceId;
use sqlx::Row;
use time::OffsetDateTime;

use super::PostgresAutopilotRepository;
use super::evidence;
use super::map_sqlx;
use crowdrelay_application::RepositoryError;

/// Processes a batch of pending attribution requests. Claims pending
/// requests, discovers competing actions, runs the allocator, and writes
/// credited entries. Returns the number of requests processed.
pub(in crate::autopilot) async fn process_attribution_batch(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    batch_size: u32,
) -> Result<u32, RepositoryError> {
    let pool = &repo.pool;
    // Claim pending attribution requests.
    let rows = sqlx::query(
        r#"
        UPDATE viryaos_attribution_requests
        SET status = 'processing',
            attempt_count = attempt_count + 1
        WHERE id IN (
            SELECT id FROM viryaos_attribution_requests
            WHERE workspace_id = $1
              AND status = 'pending'
            ORDER BY created_at ASC
            LIMIT $2
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, measurement_id, action_id, attribution_version
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(batch_size as i64)
    .fetch_all(pool)
    .await
    .map_err(super::map_sqlx)?;
    if rows.is_empty() {
        return Ok(0);
    }
    let allocator = ProportionalCreditAllocator;
    let mut processed = 0u32;
    for row in &rows {
        let request_id: uuid::Uuid = row.try_get("id").unwrap_or_default();
        let measurement_id: uuid::Uuid = row.try_get("measurement_id").unwrap_or_default();
        let action_id: uuid::Uuid = row.try_get("action_id").unwrap_or_default();
        let attribution_version: i32 = row.try_get("attribution_version").unwrap_or(1);
        match process_one(
            repo,
            &allocator,
            workspace_id,
            measurement_id,
            action_id,
            attribution_version as u32,
        )
        .await
        {
            Ok(()) => {
                mark_done(pool, measurement_id).await;
                processed += 1;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    %request_id,
                    %measurement_id,
                    "attribution request failed"
                );
                let _ = sqlx::query(
                    r#"
                    UPDATE viryaos_attribution_requests
                    SET status = 'pending', last_error = $2
                    WHERE id = $1 AND status = 'processing'
                    "#,
                )
                .bind(request_id)
                .bind(format!("{e}"))
                .execute(pool)
                .await;
            }
        }
    }
    Ok(processed)
}

async fn process_one(
    repo: &PostgresAutopilotRepository,
    allocator: &ProportionalCreditAllocator,
    workspace_id: WorkspaceId,
    measurement_id: uuid::Uuid,
    action_id: uuid::Uuid,
    attribution_version: u32,
) -> Result<(), RepositoryError> {
    let pool = &repo.pool;
    // Load the outcome from the evidence table.
    let outcome_row = sqlx::query(
        r#"
        SELECT
            observed_incremental_fans,
            durable_fans_30d,
            timestamp,
            resolved_at
        FROM viryaos_growth_evidence
        WHERE workspace_id = $1
          AND action_id = $2
          AND resolved_at IS NOT NULL
        LIMIT 1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .fetch_optional(pool)
    .await
    .map_err(super::map_sqlx)?;
    let outcome_row = match outcome_row {
        Some(r) => r,
        None => return Ok(()), // No resolved evidence yet — nothing to attribute.
    };
    use sqlx::Row;
    let observed_incremental: Option<f64> = outcome_row
        .try_get("observed_incremental_fans")
        .ok()
        .flatten();
    let durable_fans_30d: Option<f64> = outcome_row.try_get("durable_fans_30d").ok().flatten();
    let timestamp: OffsetDateTime = outcome_row
        .try_get("timestamp")
        .unwrap_or_else(|_| OffsetDateTime::now_utc());
    let resolved_at: Option<OffsetDateTime> = outcome_row.try_get("resolved_at").ok().flatten();
    let observed = observed_incremental.unwrap_or(0.0);
    if observed.abs() < 0.001 {
        return Ok(()); // No incremental fans — nothing to attribute.
    }
    let window_end = resolved_at.unwrap_or_else(|| timestamp + time::Duration::days(14));
    let window_start = timestamp;
    // Discover competing actions.
    let competing = evidence::discover_competing_actions(
        repo,
        workspace_id,
        action_id,
        window_start,
        window_end,
    )
    .await?;
    // Construct the FanOutcome.
    let outcome = FanOutcome {
        workspace_id: workspace_id.into_uuid(),
        observed_incremental_fans: observed,
        durable_fans_30d,
        measurement_window_start: window_start,
        measurement_window_end: window_end,
    };
    // Run the allocator.
    let mut result = allocator.allocate(&outcome, &competing);
    // Upgrade the credits whose action was a clean randomized treatment.
    //
    // The allocator sets `is_causal_evidence: false` on every credit and
    // documents the upgrade as this worker's job — "true only when the
    // experiment assignment's final_evidence_quality = 'randomized_holdout'
    // and final_contamination < 0.1" — and the upgrade was never written. So
    // the flag migration 0176 added to separate attribution artifacts from
    // causal claims has been constant `false` since it landed, and the
    // community-engager holdout now running would have filed its first real
    // experimental results as ordinary proportional attribution.
    mark_causal_credits(pool, workspace_id, &mut result).await?;
    // Write the credit ledger entries (idempotent).
    evidence::record_credit_allocation(
        repo,
        workspace_id,
        &outcome,
        &result,
        Some(measurement_id),
        attribution_version,
    )
    .await?;
    Ok(())
}

/// Sets `is_causal_evidence` on credits backed by a clean randomized
/// treatment assignment.
///
/// Both conditions matter and neither is sufficient alone. A randomized
/// assignment contaminated by concurrent actions on the same unit is not a
/// clean experiment, and a clean assignment that was never randomized is not
/// an experiment at all. `final_contamination` is NULL until the measurement
/// resolves it, and NULL is not "clean" — an unevaluated assignment stays
/// non-causal, because the flag exists to mark what has been established.
async fn mark_causal_credits(
    pool: &sqlx::PgPool,
    workspace_id: WorkspaceId,
    result: &mut crowdrelay_brain::AttributionResult,
) -> Result<(), RepositoryError> {
    if result.credits.is_empty() {
        return Ok(());
    }
    let action_ids: Vec<uuid::Uuid> = result.credits.iter().map(|c| c.action_id).collect();
    let causal: Vec<uuid::Uuid> = sqlx::query_scalar(
        r#"
        SELECT action_id
        FROM viryaos_experiment_assignments
        WHERE workspace_id = $1
          AND action_id = ANY($2)
          AND arm = 'treatment'
          AND final_evidence_quality = 'randomized_holdout'
          AND final_contamination IS NOT NULL
          AND final_contamination < 0.1
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&action_ids)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx)?;
    if causal.is_empty() {
        return Ok(());
    }
    for credit in &mut result.credits {
        if causal.contains(&credit.action_id) {
            credit.is_causal_evidence = true;
        }
    }
    Ok(())
}

async fn mark_done(pool: &sqlx::PgPool, measurement_id: uuid::Uuid) {
    let _ = sqlx::query(
        r#"
        UPDATE viryaos_attribution_requests
        SET status = 'done', processed_at = now()
        WHERE measurement_id = $1 AND status = 'processing'
        "#,
    )
    .bind(measurement_id)
    .execute(pool)
    .await;
}
