//! Getting play and wave outcomes out of `processing` when their worker died.
//!
//! Both claims commit `status = 'processing'` on their own, then measure, then
//! settle in a later statement. A worker that stops in between — and every
//! blue-green deploy stops one — used to leave the row `processing` for good:
//! no sweep selected it again, so the outcome was never measured and the play
//! or wave never taught the brain anything. Measurements already recover this
//! way (`measurement.rs`); these two tables did not.
//!
//! The rule mirrors the play outcome's own failure path: a row gets five
//! attempts. A stale row with attempts left goes back to `pending` and is
//! claimable at once; a stale row on its fifth attempt is `failed`, because a
//! measurement that has now died five times will not succeed on a sixth.

use super::*;

/// How long a claim may sit in `processing` before it is presumed dead. A
/// measurement reads a handful of rows; fifteen minutes is the same margin
/// `autopilot_measurements` uses.
const STALE_AFTER_MINUTES: i32 = 15;

/// Attempts an outcome gets in total, counting the claim that went stale.
/// Matches the retryable-failure cap in `play_outcomes.rs`.
pub(super) const MAX_OUTCOME_ATTEMPTS: i32 = 5;

pub(super) async fn recover_stale_play_outcomes(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    sqlx::query(
        r#"
        UPDATE play_outcomes
        SET status = CASE WHEN attempt_count < $3 THEN 'pending' ELSE 'failed' END,
            available_at = CASE WHEN attempt_count < $3 THEN $2 ELSE available_at END,
            started_at = CASE WHEN attempt_count < $3 THEN NULL ELSE started_at END,
            finished_at = CASE WHEN attempt_count < $3 THEN NULL ELSE $2 END,
            last_error_kind = CASE WHEN attempt_count < $3
                THEN 'stale_processing_recovered' ELSE 'stale_retry_exhausted' END
        WHERE workspace_id = $1
          AND status = 'processing'
          AND started_at <= $2 - make_interval(mins => $4)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(MAX_OUTCOME_ATTEMPTS)
    .bind(STALE_AFTER_MINUTES)
    .execute(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

pub(super) async fn recover_stale_wave_outcomes(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    sqlx::query(
        r#"
        UPDATE outreach_wave_outcomes
        SET status = CASE WHEN attempt_count < $3 THEN 'pending' ELSE 'failed' END,
            available_at = CASE WHEN attempt_count < $3 THEN $2 ELSE available_at END,
            started_at = CASE WHEN attempt_count < $3 THEN NULL ELSE started_at END,
            finished_at = CASE WHEN attempt_count < $3 THEN NULL ELSE $2 END,
            last_error_kind = CASE WHEN attempt_count < $3
                THEN 'stale_processing_recovered' ELSE 'stale_retry_exhausted' END,
            last_error_retryable = attempt_count < $3
        WHERE workspace_id = $1
          AND status = 'processing'
          AND started_at <= $2 - make_interval(mins => $4)
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(now)
    .bind(MAX_OUTCOME_ATTEMPTS)
    .bind(STALE_AFTER_MINUTES)
    .execute(pool)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}
