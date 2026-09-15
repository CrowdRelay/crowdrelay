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
    // Claim pending attribution requests — and re-claim stale `processing`
    // ones. The claim commits in its own statement, so a worker that dies
    // between claiming and resolving leaves the row `processing` forever:
    // this query is the only place `pending` is selected, and the module's
    // crash-recovery promise (a request survives the worker) is void
    // without the second disjunct. An hour is far beyond any real batch's
    // processing time; `created_at` is the claim's own clock since the
    // table carries no `updated_at`.
    let rows = sqlx::query(
        r#"
        UPDATE viryaos_attribution_requests
        SET status = 'processing',
            attempt_count = attempt_count + 1
        WHERE id IN (
            SELECT id FROM viryaos_attribution_requests
            WHERE workspace_id = $1
              AND (status = 'pending'
                   OR (status = 'processing'
                       AND created_at < now() - INTERVAL '1 hour'))
            ORDER BY created_at ASC
            LIMIT $2
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, measurement_id, action_id, attribution_version, attempt_count
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
        // A claimed row that cannot decode is a schema disagreement, not
        // something a nil UUID should pretend to process: the row stays
        // `processing`, the stale-claim disjunct requeues it in an hour,
        // and the log says why instead of the row silently vanishing.
        let (Ok(request_id), Ok(measurement_id), Ok(action_id), Ok(attribution_version)) = (
            row.try_get::<uuid::Uuid, _>("id"),
            row.try_get::<uuid::Uuid, _>("measurement_id"),
            row.try_get::<uuid::Uuid, _>("action_id"),
            row.try_get::<i32, _>("attribution_version"),
        ) else {
            tracing::error!("claimed attribution request row failed to decode");
            continue;
        };
        let attempts: i32 = row.try_get("attempt_count").unwrap_or(0);
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
                mark_done(pool, request_id).await;
                processed += 1;
            }
            Err(e) => {
                let status = failure_status(&e, attempts);
                tracing::warn!(
                    error = %e,
                    %request_id,
                    %measurement_id,
                    status,
                    "attribution request failed"
                );
                let writeback = sqlx::query(
                    r#"
                    UPDATE viryaos_attribution_requests
                    SET status = $3, last_error = $2
                    WHERE id = $1 AND status = 'processing'
                    "#,
                )
                .bind(request_id)
                .bind(format!("{e}"))
                .bind(status)
                .execute(pool)
                .await;
                if let Err(writeback_error) = writeback {
                    // The row stays `processing` and re-claims in an hour —
                    // but a writeback that also fails is the moment not to
                    // be quiet about.
                    tracing::error!(
                        error = %writeback_error,
                        %request_id,
                        "attribution failure could not be recorded"
                    );
                }
            }
        }
    }
    Ok(processed)
}

/// Maps a processing failure to the request's next status.
///
/// A permanent verdict retried forever is a spin loop wearing a retry's
/// clothes: `Conflict` means the write can never apply to this state,
/// `NotFound` means the rows it joins are gone, and no poll interval turns
/// either into success. Two production requests passed 2,000 attempts each
/// doing exactly that. Transient classes still return to `pending`, but no
/// error may retry without bound — `MAX_ATTRIBUTION_ATTEMPTS` lands every
/// class in `failed` eventually, where an operator can see it.
fn failure_status(error: &RepositoryError, attempt_count: i32) -> &'static str {
    let terminal = matches!(
        error,
        RepositoryError::Conflict | RepositoryError::ConflictBecause(_) | RepositoryError::NotFound
    );
    if terminal || attempt_count >= MAX_ATTRIBUTION_ATTEMPTS {
        "failed"
    } else {
        "pending"
    }
}

const MAX_ATTRIBUTION_ATTEMPTS: i32 = 50;

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
    let competing = evidence::discover_competing_actions(
        repo,
        workspace_id,
        action_id,
        window_start,
        window_end,
    )
    .await?;
    let outcome = FanOutcome {
        workspace_id: workspace_id.into_uuid(),
        observed_incremental_fans: observed,
        durable_fans_30d,
        measurement_window_start: window_start,
        measurement_window_end: window_end,
    };
    let mut result = allocator.allocate(&outcome, &competing);
    // Upgrade the credits whose action was a clean randomized treatment.
    //
    // The allocator sets `is_causal_evidence: false` on every credit and
    // documents the upgrade as this worker's job — true only when the
    // experiment assignment's final_evidence_quality is 'randomized_holdout'
    // and its contamination is under the ceiling — and the upgrade was never
    // written. So
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
///
/// The threshold is bound from [`CONTAMINATION_CEILING`] rather than written
/// as a literal. That constant's own doc claims "one number, three call sites,
/// so they cannot drift into disagreeing about what clean means" — and this,
/// the site that decides whether a credit may be called *causal*, was a `0.1`
/// in a SQL string that happened to match. Identical today; the drift the
/// constant exists to prevent is exactly the drift a literal permits.
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
          AND final_contamination < $3
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&action_ids)
    .bind(crowdrelay_brain::CONTAMINATION_CEILING)
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

/// Marks the request that was actually processed. Scoping by id, not by
/// measurement: requests are versioned, so two versions of the same
/// measurement can be claimed in one batch, and a sweep by measurement_id
/// would mark the newer version done before its credits were written — and
/// its own failure update would then find no `processing` row to move.
async fn mark_done(pool: &sqlx::PgPool, request_id: uuid::Uuid) {
    let result = sqlx::query(
        r#"
        UPDATE viryaos_attribution_requests
        SET status = 'done', processed_at = now()
        WHERE id = $1 AND status = 'processing'
        "#,
    )
    .bind(request_id)
    .execute(pool)
    .await;
    if let Err(error) = result {
        // A success that cannot be recorded is not a failure to process —
        // the credits are written and the stale-claim disjunct will run the
        // request again, idempotently. But silence here once hid a bug, so
        // the row's second trip is at least announced.
        tracing::error!(error = %error, %request_id, "attribution success could not be recorded");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Permanent verdicts terminate; transient ones retry; nothing retries
    /// without bound. The two production rows that passed 2,000 attempts on
    /// a guaranteed-forever conflict are the receipt for why `pending` is not
    /// a valid answer to every error.
    #[test]
    fn failure_status_lands_permanent_verdicts_and_bounds_the_rest() {
        for error in [
            RepositoryError::Conflict,
            RepositoryError::ConflictBecause("still stale"),
            RepositoryError::NotFound,
        ] {
            assert_eq!(failure_status(&error, 1), "failed");
        }
        for error in [RepositoryError::Unavailable, RepositoryError::Unexpected] {
            assert_eq!(failure_status(&error, 1), "pending");
            assert_eq!(failure_status(&error, MAX_ATTRIBUTION_ATTEMPTS), "failed");
        }
        assert_eq!(failure_status(&RepositoryError::Conflict, 0), "failed");
    }
}
