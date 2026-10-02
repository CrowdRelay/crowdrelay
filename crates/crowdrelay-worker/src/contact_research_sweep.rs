//! Sends the research agent after the people the outreach engine would pitch if
//! only the band had read them.
//!
//! The rule: nobody is pitched unread. `evaluate_outreach` holds a target with
//! no recent, sourced fact on file (`NeedsResearch`), so without something to
//! close the loop those pitches would wait forever. This is that something: a
//! slow, bounded sweep that queues `contact-researcher` tasks.
//!
//! What it decides and what it does not:
//!
//! * It decides **who is worth researching**, which costs a premium agent run.
//!   It picks open targets with a live opportunity, no answer yet, and no fact
//!   on file. The *contact* decision stays with `evaluate_outreach`; if this
//!   over-picks, the cost is a research run, never a mail.
//! * It never contacts anybody. It writes a task row, nothing else.
//! * It is capped: a handful per pass and a hard daily ceiling, so a large
//!   backlog is worked down over days instead of spent in an hour. Ordered by
//!   how relevant the opportunity is, so the budget goes to the pitches that
//!   matter most.
//! * Off unless `CROWDRELAY_CONTACT_RESEARCH_SWEEP` is true: it spends
//!   premium-model money, and that is the operator's decision to make once.

use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::contact_research::{ResearchError, queue_target_research};
use sqlx::PgPool;
use time::OffsetDateTime;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

/// How often the sweep looks.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// New research tasks per pass.
pub const PER_PASS: i64 = 5;
/// New research tasks per rolling day. A hard ceiling, not a target.
pub const PER_DAY: i64 = 15;

#[derive(Debug, thiserror::Error)]
pub enum SweepError {
    #[error("contact research sweep database operation failed")]
    Database(#[from] sqlx::Error),
}

#[derive(Clone, Debug)]
pub struct ContactResearchSweep {
    pool: PgPool,
    workspace_id: WorkspaceId,
    operation_timeout: Duration,
}

impl ContactResearchSweep {
    #[must_use]
    pub fn new(pool: PgPool, workspace_id: WorkspaceId, operation_timeout: Duration) -> Self {
        Self {
            pool,
            workspace_id,
            operation_timeout,
        }
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = interval(SWEEP_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    match timeout(self.operation_timeout, self.run_once()).await {
                        Ok(Ok(queued)) if queued > 0 => {
                            tracing::info!(queued, "contact research sweep sent the agent after unread targets");
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => tracing::warn!(%error, "contact research sweep failed"),
                        Err(_) => tracing::warn!("contact research sweep timed out"),
                    }
                }
            }
        }
    }

    /// One pass: the targets worth reading, most relevant first, within the
    /// day's remaining budget. Public so tests drive the real queries.
    ///
    /// # Errors
    ///
    /// Propagates the database error. A target the queue refuses (read
    /// meanwhile, researched this week, no longer open) is skipped, not an
    /// error: the sweep's list is a suggestion and the queue is the authority.
    pub async fn run_once(&self) -> Result<usize, SweepError> {
        let ws = self.workspace_id.into_uuid();
        let table_exists =
            sqlx::query_scalar::<_, bool>("SELECT to_regclass('agent_service_tasks') IS NOT NULL")
                .fetch_one(&self.pool)
                .await?;
        if !table_exists {
            return Ok(0);
        }
        let sent_today = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM agent_service_tasks
             WHERE workspace_id = $1 AND template_id = $2
               AND created_at > now() - interval '24 hours'",
        )
        .bind(ws)
        .bind(crowdrelay_infra::contact_research::RESEARCH_TEMPLATE)
        .fetch_one(&self.pool)
        .await?;
        let budget = (PER_DAY - sent_today).clamp(0, PER_PASS);
        if budget == 0 {
            return Ok(0);
        }
        let candidates = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT target.id
            FROM outreach_targets AS target
            JOIN LATERAL (
                SELECT max(opportunity.relevance_basis_points) AS relevance
                FROM outreach_opportunities AS opportunity
                WHERE opportunity.workspace_id = target.workspace_id
                  AND opportunity.target_id = target.id
                  AND opportunity.active
                  AND opportunity.expires_at > now()
            ) AS live ON live.relevance IS NOT NULL
            WHERE target.workspace_id = $1
              AND target.active
              AND target.verified
              AND target.accepts_outreach
              AND NOT target.do_not_contact
              -- Nobody who has answered: they get a person's reply, not research.
              AND target.last_reply_disposition = 'none'
              AND target.target_kind IN
                  ('playlist','radio','press','creator','support_slot','endorsement',
                   'media_patronage','organiser')
              AND NOT EXISTS (
                  SELECT 1 FROM contact_research AS research
                  WHERE research.workspace_id = target.workspace_id
                    AND research.normalized_email = lower(btrim(target.contact_email))
                    AND research.observed_on >= (now() AT TIME ZONE 'UTC')::date - 120
                  AND research.praise IS NOT NULL
                  AND char_length(btrim(research.praise)) >= 40
              )
            ORDER BY live.relevance DESC, target.id
            LIMIT $2
            "#,
        )
        .bind(ws)
        // Over-fetch: the queue refuses some (researched this week), and a
        // refusal must not eat the pass.
        .bind(budget.saturating_mul(4))
        .fetch_all(&self.pool)
        .await?;

        let now = OffsetDateTime::now_utc();
        let mut queued = 0_i64;
        for target_id in candidates {
            if queued >= budget {
                break;
            }
            match queue_target_research(&self.pool, ws, target_id, now).await {
                Ok(_) => queued += 1,
                Err(ResearchError::Database(error)) => return Err(error.into()),
                Err(ResearchError::Refused(_) | ResearchError::NotFound) => {}
            }
        }
        Ok(usize::try_from(queued).unwrap_or(0))
    }
}
