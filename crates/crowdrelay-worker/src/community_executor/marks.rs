//! Status writes on `community_posts` rows: failed, deferred, rate-limited.
//!
//! Every terminal/deferred write funnels through these so the batch-liveness
//! guard, the transient-attempt cap, and the parent-action propagation stay
//! in one place rather than repeated at each call site.

use super::*;

impl CommunityExecutorWorker {
    /// Marks a community post as failed with an error message and
    /// propagates the failure back to the parent autopilot action so the
    /// ledger does not report success for a post that never went live.
    pub(super) async fn mark_failed(
        &self,
        post_id: Uuid,
        error: &str,
    ) -> Result<(), CommunityExecutorError> {
        sqlx::query(
            r#"
            UPDATE community_posts
            SET status = 'failed',
                error_message = $2,
                updated_at = now()
            WHERE id = $1
              AND workspace_id = $3
            "#,
        )
        .bind(post_id)
        .bind(error)
        .bind(self.workspace_id.into_uuid())
        .execute(&self.pool)
        .await?;
        self.propagate_failure(post_id, error).await
    }

    /// Tells the parent autopilot action, and the experiment assignment, that
    /// the post did not go live.
    ///
    /// Split out of `mark_failed` because a transient failure defers the draft
    /// without saying anything to the ledger — the action is terminal once told,
    /// so propagating on the first network error made one outage look like a
    /// batch of failed posts to the brain.
    async fn propagate_failure(
        &self,
        post_id: Uuid,
        error: &str,
    ) -> Result<(), CommunityExecutorError> {
        let action_id: Option<Uuid> = sqlx::query_scalar(
            "SELECT action_id FROM community_posts WHERE id = $1 AND workspace_id = $2",
        )
        .bind(post_id)
        .bind(self.workspace_id.into_uuid())
        .fetch_optional(&self.pool)
        .await?;

        // The action was marked 'succeeded' by actions_execution.rs before this
        // worker ran — that was premature. Correct it now so the operator sees
        // the real outcome in the autopilot ledger.
        if let Some(action_id) = action_id {
            let error_kind = if error.len() > 96 {
                "community_post_failed"
            } else {
                error
            };
            sqlx::query(
                r#"
                UPDATE autopilot_actions
                SET status = 'failed',
                    finished_at = now(),
                    last_error_kind = $3,
                    updated_at = now()
                WHERE id = $1 AND workspace_id = $2 AND status = 'succeeded'
                "#,
            )
            .bind(action_id)
            .bind(self.workspace_id.into_uuid())
            .bind(error_kind)
            .execute(&self.pool)
            .await?;
            // Transition the experiment assignment execution_status from
            // dispatched → failed. The external intervention was attempted
            // but definitively failed. Monotonic: only dispatched → failed
            // is allowed; if the assignment is not in 'dispatched' state,
            // this is a no-op.
            sqlx::query(
                r#"
                UPDATE experiment_assignments
                SET execution_status = 'failed',
                    trace_id = COALESCE(trace_id, (SELECT trace_id FROM autopilot_actions WHERE id = $2))
                WHERE workspace_id = $1
                  AND action_id = $2
                  AND execution_status = 'dispatched'
                "#,
            )
            .bind(self.workspace_id.into_uuid())
            .bind(action_id)
            .execute(&self.pool)
            .await?;
            tracing::warn!(
                post_id = %post_id,
                action_id = %action_id,
                error = %error,
                "community post failed — propagated failure to autopilot action"
            );
        }
        Ok(())
    }

    /// Defers a draft after a failure that a later cycle may not hit.
    ///
    /// Bounded by `MAX_TRANSIENT_ATTEMPTS`: past that the draft is given up on
    /// and the parent action is told, because a condition that has not resolved
    /// in six attempts is not transient. `attempts` is incremented at claim time,
    /// so the count is already accurate here.
    ///
    /// The decision is made in SQL so the read and the write cannot disagree
    /// about the count.
    pub(super) async fn mark_transient_failure(
        &self,
        post_id: Uuid,
        error: &str,
    ) -> Result<(), CommunityExecutorError> {
        let exhausted: bool = sqlx::query_scalar(
            r#"
            UPDATE community_posts
            SET status = CASE
                    WHEN EXISTS (
                        SELECT 1 FROM community_relay_batches rb
                        WHERE rb.workspace_id = community_posts.workspace_id
                          AND rb.source_id = community_posts.relay_source_id
                          AND rb.status IN ('revoked', 'done')
                    ) THEN 'cancelled'
                    WHEN attempts >= $3 THEN 'failed'
                    ELSE 'rate_limited'
                END,
                rate_limited_until = CASE
                    WHEN EXISTS (
                        SELECT 1 FROM community_relay_batches rb
                        WHERE rb.workspace_id = community_posts.workspace_id
                          AND rb.source_id = community_posts.relay_source_id
                          AND rb.status IN ('revoked', 'done')
                    ) OR attempts >= $3 THEN NULL
                    ELSE now() + make_interval(secs => $4::double precision)
                END,
                error_message = $2,
                updated_at = now()
            WHERE id = $1
              AND workspace_id = $5
            RETURNING status = 'failed'
            "#,
        )
        .bind(post_id)
        .bind(error)
        .bind(MAX_TRANSIENT_ATTEMPTS)
        .bind(RATE_LIMIT_BACKOFF.as_secs() as i64)
        .bind(self.workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await?;
        if exhausted {
            // Only now is the parent action told. Propagating on the first
            // transient failure is what made one outage look like a batch of
            // failed posts to the brain.
            self.propagate_failure(post_id, error).await?;
        }
        Ok(())
    }

    /// Marks a community post as rate-limited with a backoff window.
    /// Does not propagate to the autopilot action because rate-limited
    /// posts will be retried — the action stays 'succeeded' and the
    /// community_posts row tracks the retry state.
    pub(super) async fn mark_rate_limited(
        &self,
        post_id: Uuid,
        backoff: Duration,
    ) -> Result<(), CommunityExecutorError> {
        sqlx::query(
            r#"
            UPDATE community_posts
            SET status = CASE
                    WHEN EXISTS (
                        SELECT 1 FROM community_relay_batches rb
                        WHERE rb.workspace_id = community_posts.workspace_id
                          AND rb.source_id = community_posts.relay_source_id
                          AND rb.status IN ('revoked', 'done')
                    ) THEN 'cancelled'
                    ELSE 'rate_limited' END,
                rate_limited_until = CASE
                    WHEN EXISTS (
                        SELECT 1 FROM community_relay_batches rb
                        WHERE rb.workspace_id = community_posts.workspace_id
                          AND rb.source_id = community_posts.relay_source_id
                          AND rb.status IN ('revoked', 'done')
                    ) THEN NULL
                    ELSE now() + make_interval(secs => $2::double precision) END,
                updated_at = now()
            WHERE id = $1
              AND workspace_id = $3
            "#,
        )
        .bind(post_id)
        .bind(backoff.as_secs() as i64)
        .bind(self.workspace_id.into_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
