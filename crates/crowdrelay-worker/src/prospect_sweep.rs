//! Turns people who already spoke to the band in public into prospects, and
//! lets the ones who never progress expire.
//!
//! The first source is the deliberately healthiest one: commenters under a post
//! on a surface the band controls. They addressed the band; the band answering
//! in the same thread is the expected reply. This sweep does nothing to them —
//! it records who they are, what they said (verbatim, bounded) and where, so
//! the next-best-action evaluator has evidence to read. It never contacts
//! anyone and never writes a `fans` row: a commenter is a prospect, and the only
//! road to `fans` is the person joining through a tracked, consented path.
//!
//! Two bounded jobs per pass, in this order:
//!
//! 1. **Observe.** Comments from the last 30 days, newest first, capped. A
//!    comment already on file (same source, same comment id) is a no-op, and a
//!    prospect that has said no or is suppressed is not collected against — see
//!    `crowdrelay_infra::fan_prospects::observe`.
//! 2. **Expire.** Prospects past their retention without progress are deleted
//!    with their evidence. A prospect read again this pass had its deadline
//!    moved first, so a person still talking to the band is never expired out
//!    from under a conversation.

use std::time::Duration;

use crowdrelay_domain::{
    WorkspaceId,
    fan_prospect::{ObservationKind, ProspectSource},
};
use crowdrelay_infra::fan_prospects::{
    ObserveOutcome, ObservedPerson, ProspectError, expire, observe,
};
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Comments read per pass. A backlog is worked down over passes, newest first.
pub const OBSERVE_PER_PASS: i64 = 500;
/// Prospects deleted per pass.
pub const EXPIRE_PER_PASS: i64 = 500;
/// How far back a comment is still read. Older than this is history, not a
/// signal the evaluator should act on.
const LOOKBACK_DAYS: i32 = 30;
/// A bare comment is weak evidence of anything; the evaluator needs more than
/// one to qualify a person. Basis points.
const COMMENT_CONFIDENCE: u16 = 3_000;

#[derive(Debug, thiserror::Error)]
pub enum SweepError {
    #[error("prospect sweep database operation failed")]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Prospect(#[from] ProspectError),
}

/// What one pass did.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct SweepReport {
    pub created: u64,
    pub appended: u64,
    pub already_known: u64,
    pub not_collected: u64,
    pub not_an_identity: u64,
    pub expired: u64,
}

#[derive(Debug, FromRow)]
struct CommentRow {
    id: Uuid,
    platform: String,
    author: String,
    body: String,
    created_at: OffsetDateTime,
    source_url: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ProspectSweep {
    pool: PgPool,
    workspace_id: WorkspaceId,
    operation_timeout: Duration,
}

impl ProspectSweep {
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
                    match timeout(self.operation_timeout * 6, self.run_once(OffsetDateTime::now_utc())).await {
                        Ok(Ok(report)) if report != SweepReport::default() => {
                            tracing::info!(?report, "prospect sweep");
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => tracing::warn!(%error, "prospect sweep failed"),
                        Err(_) => tracing::warn!("prospect sweep timed out"),
                    }
                }
            }
        }
    }

    /// One pass. Public so tests drive the real queries with their own clock.
    ///
    /// # Errors
    ///
    /// Propagates the database error. One bad comment never stops the pass: an
    /// unusable author is counted as `not_an_identity` and the rest proceed.
    pub async fn run_once(&self, now: OffsetDateTime) -> Result<SweepReport, SweepError> {
        let ws = self.workspace_id.into_uuid();
        let mut report = SweepReport::default();
        let comments = sqlx::query_as::<_, CommentRow>(
            "SELECT c.id, c.platform, c.author, c.body, c.created_at,
                    CASE WHEN cs.metadata->>'url' ~ '^https?://'
                         THEN cs.metadata->>'url' END AS source_url
             FROM community_comments c
             LEFT JOIN content_sources cs
               ON cs.workspace_id = c.workspace_id AND cs.id = c.content_source_id
             WHERE c.workspace_id = $1
               AND c.created_at >= $2 - make_interval(days => $3)
               AND btrim(c.author) <> ''
             ORDER BY c.created_at DESC
             LIMIT $4",
        )
        .bind(ws)
        .bind(now)
        .bind(LOOKBACK_DAYS)
        .bind(OBSERVE_PER_PASS)
        .fetch_all(&self.pool)
        .await?;
        for comment in &comments {
            let id = comment.id.to_string();
            let outcome = observe(
                &self.pool,
                ws,
                &ObservedPerson {
                    source: ProspectSource::OwnComments,
                    platform: &comment.platform,
                    handle: &comment.author,
                    display_name: None,
                    profile_url: None,
                    kind: ObservationKind::ActiveUnderOurPost,
                    source_ref: &id,
                    source_url: comment.source_url.as_deref(),
                    observed_at: comment.created_at,
                    evidence: &comment.body,
                    confidence_basis_points: COMMENT_CONFIDENCE,
                },
            )
            .await?;
            match outcome {
                ObserveOutcome::Created { .. } => report.created += 1,
                ObserveOutcome::Known { appended: true, .. } => report.appended += 1,
                ObserveOutcome::Known {
                    appended: false, ..
                } => report.already_known += 1,
                ObserveOutcome::NotCollected { .. } => report.not_collected += 1,
                ObserveOutcome::NotAnIdentity => report.not_an_identity += 1,
            }
        }
        report.expired = expire(&self.pool, ws, now, EXPIRE_PER_PASS).await?;
        Ok(report)
    }
}
