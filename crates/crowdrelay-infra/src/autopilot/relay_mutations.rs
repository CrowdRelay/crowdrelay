//! Community relay batch mutations, split out of `control_mutations.rs`.
//!
//! The batch is the operator's one answer for a whole spread of community
//! deliveries: approve releases every parked delivery under the drip
//! interval, revoke cancels whatever has not yet left. Both run under the
//! usual operator-action idempotency ledger.

use super::control_mutations::OUTWARD_HOLD_SECONDS;
use super::*;

impl PostgresAutopilotRepository {
    /// Approves a community relay batch — the content's whole spread at once.
    ///
    /// The thing parked in front of the operator is the question "does this
    /// post go to the communities that will take it", asked once per source
    /// rather than once per community. Approving writes the standing answer
    /// on the batch row — drafts still landing queue under it without asking
    /// again — and releases every delivery already parked for the source.
    /// Released rungs drip out at the batch's `interval_seconds`, the cadence
    /// the operator saw on the card; the community executor owns that pacing.
    pub(super) async fn approve_community_relay_operator(
        &self,
        workspace_id: WorkspaceId,
        source_id: uuid::Uuid,
        interval_seconds: Option<i32>,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            let replay = operator_actions::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "approve_community_relay_batch",
                "content_source",
                source_id,
                "admin_api_key",
                idempotency_key,
                request_id,
                &json!({"requested_status": "approved"}),
            )
            .await?;
            if let Some(existing) = replay {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: source_id,
                    status: "approved".to_owned(),
                    replayed: true,
                });
            }
            // The batch is the approval's subject — no batch means no drafts
            // ever landed for this source, and there is nothing to approve.
            // An answered batch is a conflict, not a second approval.
            let batch_status = sqlx::query_scalar::<_, String>(
                "UPDATE community_relay_batches \
                 SET status = 'approved', approved_at = now(), \
                     approved_by = 'operator:community_relay', \
                     observe_until = now() + INTERVAL '7 days', \
                     interval_seconds = COALESCE($3, interval_seconds), \
                     updated_at = now() \
                 WHERE workspace_id = $1 AND source_id = $2 \
                   AND status = 'awaiting_approval' \
                 RETURNING status",
            )
            .bind(workspace_id.into_uuid())
            .bind(source_id)
            .bind(interval_seconds)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if batch_status.is_none() {
                let exists = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (SELECT 1 FROM community_relay_batches \
                     WHERE workspace_id = $1 AND source_id = $2)",
                )
                .bind(workspace_id.into_uuid())
                .bind(source_id)
                .fetch_one(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                return if exists {
                    Err(RepositoryError::Conflict)
                } else {
                    Err(RepositoryError::NotFound)
                };
            }
            let now = OffsetDateTime::now_utc();
            let released: Vec<Uuid> = sqlx::query_scalar(
                r#"
                UPDATE viryaos_autopilot_actions
                SET status='queued', approved_at=$3, approved_by='operator:community_relay',
                    -- O.2: an outward send waits out its hold window before a
                    -- worker may claim it — the batch is the approval, not a
                    -- shortcut past the window that makes revoking meaningful.
                    available_at = now() + CASE
                        WHEN action_class IN ('owned_audience', 'third_party', 'paid')
                        THEN make_interval(secs => $4::double precision)
                        ELSE INTERVAL '0'
                    END
                WHERE workspace_id=$1
                  AND action_kind='community.engage.request'
                  AND payload->>'source_id' = $2::text
                  AND status='awaiting_approval'
                  AND (approval_expires_at IS NULL OR approval_expires_at > $3)
                RETURNING id
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(source_id)
            .bind(now)
            .bind(OUTWARD_HOLD_SECONDS)
            .fetch_all(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            // A parked rung can carry an open crew assignment — close it, or
            // a reminder keeps asking somebody to approve what already
            // queued.
            sqlx::query(
                "UPDATE viryaos_team_assignments \
                 SET status='done', completed_at=$3, next_reminder_at=NULL \
                 WHERE workspace_id=$1 AND action_id = ANY($2) AND status='open'",
            )
            .bind(workspace_id.into_uuid())
            .bind(&released)
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: source_id,
                status: format!("approved:{}", released.len()),
                replayed: false,
            })
        })
        .await
    }

    /// Revokes a community relay batch — "stop the rest of this spread".
    ///
    /// The batch row is the standing answer, so revoking flips it and
    /// everything the answer covered: deliveries still parked lose their ask,
    /// deliveries queued but not yet executed are cancelled, and the
    /// community_posts rows the drip was about to send are cancelled rather
    /// than left claimable. A post already on Reddit keeps its record — a
    /// revoke cannot unpost, and pretending it could is the dishonest part
    /// the status names around.
    pub(super) async fn revoke_community_relay_operator(
        &self,
        workspace_id: WorkspaceId,
        source_id: uuid::Uuid,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            let replay = operator_actions::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "revoke_community_relay_batch",
                "content_source",
                source_id,
                "admin_api_key",
                idempotency_key,
                request_id,
                &json!({"requested_status": "revoked"}),
            )
            .await?;
            if let Some(existing) = replay {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: source_id,
                    status: "revoked".to_owned(),
                    replayed: true,
                });
            }
            let revoked = sqlx::query_scalar::<_, String>(
                "UPDATE community_relay_batches \
                 SET status = 'revoked', revoked_at = now(), \
                     revoked_by = 'operator:community_relay', updated_at = now() \
                 WHERE workspace_id = $1 AND source_id = $2 \
                   AND status IN ('awaiting_approval', 'approved') \
                 RETURNING status",
            )
            .bind(workspace_id.into_uuid())
            .bind(source_id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            if revoked.is_none() {
                let exists = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (SELECT 1 FROM community_relay_batches \
                     WHERE workspace_id = $1 AND source_id = $2)",
                )
                .bind(workspace_id.into_uuid())
                .bind(source_id)
                .fetch_one(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                return if exists {
                    Err(RepositoryError::Conflict)
                } else {
                    Err(RepositoryError::NotFound)
                };
            }
            let now = OffsetDateTime::now_utc();
            let cancelled: Vec<Uuid> = sqlx::query_scalar(
                "UPDATE viryaos_autopilot_actions \
                 SET status='cancelled', finished_at=$3 \
                 WHERE workspace_id=$1 \
                   AND action_kind='community.engage.request' \
                   AND payload->>'source_id' = $2::text \
                   AND status IN ('awaiting_approval', 'queued') \
                 RETURNING id",
            )
            .bind(workspace_id.into_uuid())
            .bind(source_id)
            .bind(now)
            .fetch_all(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            sqlx::query(
                "UPDATE viryaos_team_assignments \
                 SET status='cancelled', completed_at=NULL, next_reminder_at=NULL \
                 WHERE workspace_id=$1 AND action_id = ANY($2) AND status='open'",
            )
            .bind(workspace_id.into_uuid())
            .bind(&cancelled)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            // The queue rows the drip was about to send stop being claimable.
            // `posting` rows are mid-flight — a revoke cannot reach inside a
            // Reddit call, and the row's own recovery owns that outcome.
            let posts_cancelled = sqlx::query(
                "UPDATE community_posts \
                 SET status='cancelled', updated_at=$3 \
                 WHERE workspace_id=$1 AND relay_source_id=$2 \
                   AND status IN ('pending','rate_limited','awaiting_manual_post')",
            )
            .bind(workspace_id.into_uuid())
            .bind(source_id)
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id: source_id,
                status: format!(
                    "revoked:{}:{}",
                    cancelled.len(),
                    posts_cancelled.rows_affected()
                ),
                replayed: false,
            })
        })
        .await
    }
}
