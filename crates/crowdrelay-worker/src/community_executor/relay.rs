//! Community relay batch plumbing for the executor: the send-time gate,
//! the standing-grant fallback, and the observation-window sweep.
//!
//! A relay batch is the operator's one answer for a whole spread of
//! community deliveries. This module is where the executor checks that
//! answer at send time (`relay_batch_gate`), lets a live per-target grant
//! carry a delivery while the card is still unanswered
//! (`live_standing_grant`), and closes finished batches
//! (`finish_observed_batches`).

use super::*;

/// What the send-time batch gate answered for a delivery's source.
pub(super) enum RelayBatchGate {
    /// Batch approved and its interval has elapsed — the post may go out.
    Open,
    /// Batch approved but the interval has not elapsed — defer by this much.
    Defer(Duration),
    /// Batch missing, revoked or done — the delivery must not post.
    Closed,
    /// The card has not answered and nothing else covers this delivery —
    /// the grant that claimed it was revoked between claim and send. The
    /// row returns to `pending` to wait on the card, not cancelled for an
    /// answer nobody gave.
    Parked,
}

impl CommunityExecutorWorker {
    /// The send-time batch gate: is this delivery's batch still approved, and
    /// has its interval elapsed since the batch's last post?
    ///
    /// The claim answers the same question at selection time; this answers it
    /// again at the moment a post would leave. Between the two, a sibling of
    /// the same batch may have posted (the claim's snapshot predates it) or
    /// the operator may have revoked the spread — both land here.
    pub(super) async fn relay_batch_gate(
        &self,
        source_id: Uuid,
        target_id: Option<Uuid>,
    ) -> Result<RelayBatchGate, CommunityExecutorError> {
        let batch = sqlx::query_as::<_, (String, i32)>(
            "SELECT status, interval_seconds FROM community_relay_batches \
             WHERE workspace_id = $1 AND source_id = $2",
        )
        .bind(self.workspace_id.into_uuid())
        .bind(source_id)
        .fetch_optional(&self.pool)
        .await?;
        // A delivery without a live-approved batch behind it does not post:
        // missing or answered-any-other-way both mean the spread was never
        // (or is no longer) a yes.
        let Some((status, interval_seconds)) = batch else {
            return Ok(RelayBatchGate::Closed);
        };
        let authorized = match status.as_str() {
            "approved" => true,
            // The card is still unanswered, but a live standing grant for
            // this delivery's own community can carry it — the operator's
            // earlier "don't ask me about this one". The grant is re-checked
            // rather than trusting the claim's snapshot: a revocation between
            // claim and send lands exactly here.
            "awaiting_approval" => self.live_standing_grant(target_id).await?,
            // Revoked and done are the veto a grant cannot override.
            _ => false,
        };
        if !authorized {
            return Ok(if status == "awaiting_approval" {
                RelayBatchGate::Parked
            } else {
                RelayBatchGate::Closed
            });
        }
        let last_posted = sqlx::query_scalar::<_, Option<time::OffsetDateTime>>(
            r#"
            SELECT max(posted_at) FROM community_posts
            WHERE workspace_id = $1
              AND relay_source_id = $2
              AND status = 'posted'
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(source_id)
        .fetch_one(&self.pool)
        .await?;
        if let Some(at) = last_posted {
            let next_due = at + time::Duration::seconds(i64::from(interval_seconds));
            let now = time::OffsetDateTime::now_utc();
            if next_due > now {
                // The remainder of the interval, not a fresh interval —
                // deferring a full interval from now would double-wait a row
                // whose sibling posted ten minutes ago.
                let remaining = (next_due - now).whole_seconds().max(0) as u64;
                return Ok(RelayBatchGate::Defer(Duration::from_secs(remaining)));
            }
        }
        Ok(RelayBatchGate::Open)
    }

    /// Whether a live standing grant covers this delivery's community —
    /// "the operator already judged this target, stop asking".
    ///
    /// The row is read and the full `unattended_authority` rule applied —
    /// the same rule the ingest path uses. A grant is an answer only while
    /// the question it answered still stands: the `outreach` context must
    /// still be `require_approval` and the grant's own class ceiling no
    /// stricter. An operator who dialled outreach down to `observe` did not
    /// mean "keep posting where a grant exists" — the axes are re-read here
    /// rather than trusted from the claim's snapshot. A class this build
    /// cannot parse is not a grant, the same reading the ingest path gives
    /// the authority rows.
    async fn live_standing_grant(
        &self,
        target_id: Option<Uuid>,
    ) -> Result<bool, CommunityExecutorError> {
        let Some(target_id) = target_id else {
            return Ok(false);
        };
        let row: Option<(String, time::OffsetDateTime, Option<time::OffsetDateTime>)> =
            sqlx::query_as(
                r#"
            SELECT action_class, expires_at, revoked_at
            FROM standing_approvals
            WHERE workspace_id = $1
              AND action_kind = 'community.engage.request'
              AND target_key = $2
            "#,
            )
            .bind(self.workspace_id.into_uuid())
            .bind(target_id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        let Some(grant) = row.and_then(|(class, expires_at, revoked_at)| {
            ActionClass::parse(&class).map(|class| StandingGrant {
                class,
                expires_at,
                revoked_at,
            })
        }) else {
            return Ok(false);
        };
        // A community post answers to `outreach` — the effective_context
        // rule the ingest path applies. A missing or unreadable policy row
        // is the safest level on that axis, never an absent limit.
        let context_level: Option<String> = sqlx::query_scalar(
            "SELECT autonomy_level FROM autopilot_policies \
             WHERE workspace_id = $1 AND context = 'outreach' LIMIT 1",
        )
        .bind(self.workspace_id.into_uuid())
        .fetch_optional(&self.pool)
        .await?;
        let Some(context_level) = context_level.as_deref().and_then(AutonomyLevel::parse) else {
            return Ok(false);
        };
        let ceiling: Option<String> = sqlx::query_scalar(
            "SELECT ceiling FROM growth_autonomy \
             WHERE workspace_id = $1 AND action_class = $2 LIMIT 1",
        )
        .bind(self.workspace_id.into_uuid())
        .bind(grant.class.as_str())
        .fetch_optional(&self.pool)
        .await?;
        let ceiling = ceiling
            .as_deref()
            .and_then(AutonomyLevel::parse)
            .unwrap_or_else(|| grant.class.safest_ceiling());
        let authority = effective_authority(context_level, ceiling);
        Ok(
            unattended_authority(authority, Some(grant), time::OffsetDateTime::now_utc())
                == UnattendedAuthority::Grant,
        )
    }
    /// Marks approved relay batches whose observation window closed as
    /// `done`, and emits one summary event per batch — "the week answered:
    /// N posts out, M clicks back". The batch card stops reading as a plan
    /// and starts reading as the result the operator was promised when they
    /// approved it.
    pub(super) async fn finish_observed_batches(&self) -> Result<(), CommunityExecutorError> {
        let ws = self.workspace_id.into_uuid();
        let mut tx = self.pool.begin().await?;
        let finished: Vec<(Uuid, Uuid)> = sqlx::query_as(
            r#"
            UPDATE community_relay_batches
            SET status = 'done', updated_at = now()
            WHERE workspace_id = $1
              AND status = 'approved'
              AND observe_until IS NOT NULL
              AND observe_until <= now()
            RETURNING id, source_id
            "#,
        )
        .bind(ws)
        .fetch_all(&mut *tx)
        .await?;
        for (batch_id, source_id) in finished {
            // `done` closes the campaign: every delivery that never landed is
            // cancelled here so it reads as undelivered on the card instead of
            // sitting `pending` under a batch the claim lane no longer opens.
            // Their parked/queued actions close with them — an open action
            // under a done batch would dispatch, seed a post, and strand it.
            sqlx::query(
                r#"
                UPDATE community_posts
                SET status = 'cancelled', updated_at = now()
                WHERE workspace_id = $1 AND relay_source_id = $2
                  AND status IN ('pending', 'rate_limited', 'awaiting_manual_post')
                "#,
            )
            .bind(ws)
            .bind(source_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                r#"
                UPDATE autopilot_actions
                SET status = 'cancelled', finished_at = now()
                WHERE workspace_id = $1
                  AND action_kind = 'community.engage.request'
                  AND payload ->> 'source_id' = $2::text
                  AND status IN ('awaiting_approval', 'queued')
                "#,
            )
            .bind(ws)
            .bind(source_id.to_string())
            .execute(&mut *tx)
            .await?;
            let (posted, undelivered, clicks) = sqlx::query_as::<_, (i64, i64, i64)>(
                r#"
                SELECT
                    count(*) FILTER (WHERE cp.status = 'posted'),
                    count(*) FILTER (WHERE cp.status IN ('failed', 'cancelled')),
                    (SELECT count(*) FROM click_events ce
                     JOIN smart_links sl
                       ON sl.workspace_id = ce.workspace_id AND sl.id = ce.smart_link_id
                     JOIN community_posts p
                       ON p.workspace_id = sl.workspace_id
                      AND p.smart_link = '/l/' || sl.slug
                     WHERE p.workspace_id = $1 AND p.relay_source_id = $2)
                FROM community_posts cp
                WHERE cp.workspace_id = $1 AND cp.relay_source_id = $2
                "#,
            )
            .bind(ws)
            .bind(source_id)
            .fetch_one(&mut *tx)
            .await?;
            // The card reads these counts live off the batch — no outbox
            // event: nothing external consumes `relay_observed`, and an
            // unrouted event type just becomes refused webhook deliveries.
            tracing::info!(
                batch_id = %batch_id,
                source_id = %source_id,
                posted,
                undelivered,
                clicks,
                "relay batch observation window closed"
            );
        }
        tx.commit().await?;
        Ok(())
    }
}
