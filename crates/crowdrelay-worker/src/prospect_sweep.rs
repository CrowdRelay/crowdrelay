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

use std::{collections::HashSet, time::Duration};

use crowdrelay_domain::{
    WorkspaceId,
    fan_prospect::{ObservationKind, ProspectSource, classify_comment, normalize_handle},
};
use crowdrelay_infra::fan_prospects::{
    ObserveOutcome, ObservedPerson, ProspectError, TouchKind, TouchReceipt, attribute_conversions,
    expire, observe, record_touch,
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
/// A question the person put to the band in public is a stated, current ask —
/// the strongest evidence this source can give.
const QUESTION_CONFIDENCE: u16 = 8_000;

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
    /// Comments belonging to explicit staff/own-account/test identities.
    pub excluded_identity: u64,
    pub not_an_identity: u64,
    /// Comments by the tenant's own accounts (brand, owner, team): the band
    /// talking under its own post is not an audience member.
    pub own_accounts: u64,
    /// Prospects (with their evidence and touches) retracted because they turned
    /// out to be one of those accounts.
    pub own_retracted: u64,
    /// Replies the band sent that were recorded as touches this pass.
    pub touched: u64,
    /// Prospects linked to the fan their touch brought in this pass.
    pub converted: u64,
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
        let own = self.own_handles(ws).await?;
        for comment in &comments {
            if normalize_handle(&comment.author).is_some_and(|h| own.contains(&h)) {
                report.own_accounts += 1;
                continue;
            }
            let id = comment.id.to_string();
            let kind = classify_comment(&comment.body);
            let confidence = if kind == ObservationKind::ActiveUnderOurPost {
                COMMENT_CONFIDENCE
            } else {
                QUESTION_CONFIDENCE
            };
            let outcome = observe(
                &self.pool,
                ws,
                &ObservedPerson {
                    source: ProspectSource::OwnComments,
                    platform: &comment.platform,
                    platform_user_id: None,
                    handle: Some(&comment.author),
                    display_identity: &comment.author,
                    display_name: None,
                    profile_url: None,
                    kind,
                    source_ref: &id,
                    source_url: comment.source_url.as_deref(),
                    observed_at: comment.created_at,
                    evidence: &comment.body,
                    confidence_basis_points: confidence,
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
                ObserveOutcome::ExcludedIdentity => {
                    report.excluded_identity += 1;
                    // The reply worker treats a missing prospect decision as
                    // "evidence not ready" and retries later. For an explicit
                    // staff/self/test exclusion, waiting will never make the
                    // person eligible, so retire an unanswered row here rather
                    // than spending an LLM/cycle on it forever. Historical
                    // replied/skipped rows are untouched.
                    sqlx::query(
                        "UPDATE community_comments
                         SET status='skipped',
                             hold_reason='FAN SCOUT: explicit staff/own-account/test identity exclusion',
                             updated_at=now()
                         WHERE workspace_id=$1 AND id=$2 AND status='unanswered'",
                    )
                    .bind(ws)
                    .bind(comment.id)
                    .execute(&self.pool)
                    .await?;
                }
                ObserveOutcome::NotAnIdentity => report.not_an_identity += 1,
                ObserveOutcome::IdentityConflict => {
                    tracing::warn!(
                        platform = %comment.platform,
                        "prospect identity conflict; observation held"
                    );
                    report.not_an_identity += 1;
                }
            }
        }
        report.own_retracted = self.retract_own_accounts(ws, &own).await?;
        self.record_replies_as_touches(ws, &mut report).await?;
        report.converted = attribute_conversions(&self.pool, ws, now, EXPIRE_PER_PASS).await?;
        report.expired = expire(&self.pool, ws, now, EXPIRE_PER_PASS).await?;
        Ok(report)
    }

    /// Handles that are the tenant, not its audience: the `scout_own_handles`
    /// tenant setting (comma/space/newline separated — owner and team
    /// accounts the platform connections cannot know) plus the handle a
    /// connection names in its label (`Virya Instagram (@virya.official)`) or
    /// uses as its account ref (`viryaofficial`). Normalised exactly as
    /// prospect handles are, so the comparison cannot drift.
    async fn own_handles(&self, ws: Uuid) -> Result<HashSet<String>, SweepError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT value FROM tenant_settings
              WHERE workspace_id = $1 AND key = 'scout_own_handles'
             UNION ALL
             SELECT external_account_ref FROM fanbase_connections
              WHERE workspace_id = $1 AND platform IN
                    ('instagram','youtube','tiktok','bluesky','soundcloud','bandcamp','facebook','reddit','twitter','x')
                AND external_account_ref !~ '^[0-9-]+$' AND external_account_ref !~ '^r/'
             UNION ALL
             SELECT m[1] FROM fanbase_connections c,
                    LATERAL regexp_matches(c.label, '\\(@?([^)\\s]+)\\)', 'g') AS m
              WHERE c.workspace_id = $1 AND c.platform <> 'reddit'",
        )
        .bind(ws)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .flat_map(|(value,)| value.split(|c: char| c == ',' || c.is_whitespace()))
            .filter_map(normalize_handle)
            .collect())
    }

    /// A prospect that is one of the tenant's own accounts is deleted with its
    /// evidence and touches. Its touches are the band replying to the band;
    /// left in place they trip the one-voice and rate tripwires and halt the
    /// reply lane for a week over a conversation with nobody.
    async fn retract_own_accounts(
        &self,
        ws: Uuid,
        own: &HashSet<String>,
    ) -> Result<u64, SweepError> {
        if own.is_empty() {
            return Ok(0);
        }
        let handles: Vec<String> = own.iter().cloned().collect();
        let mut tx = self.pool.begin().await?;
        let gone: Vec<(Uuid,)> = sqlx::query_as(
            "DELETE FROM fan_prospects p
              WHERE p.workspace_id = $1
                AND p.status NOT IN ('converted')
                AND EXISTS (SELECT 1 FROM person_identities i
                             WHERE i.workspace_id = p.workspace_id
                               AND i.person_id = p.person_id
                               AND i.kind = 'platform_handle'
                               AND i.value = ANY($2))
          RETURNING p.person_id",
        )
        .bind(ws)
        .bind(&handles)
        .fetch_all(&mut *tx)
        .await?;
        let people: Vec<Uuid> = gone.into_iter().map(|(p,)| p).collect();
        if !people.is_empty() {
            sqlx::query(
                "DELETE FROM persons
                  WHERE workspace_id = $1 AND id = ANY($2)
                    AND NOT EXISTS (SELECT 1 FROM fan_prospects p
                                     WHERE p.workspace_id = persons.workspace_id
                                       AND p.person_id = persons.id)",
            )
            .bind(ws)
            .bind(&people)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(u64::try_from(people.len()).unwrap_or(u64::MAX))
    }

    /// The reply lane's sends, recorded as what they are: the band speaking to
    /// a prospect. A reply that carried the tenant's join link is an
    /// invitation (and is the only link a conversion can be attributed to);
    /// any other reply is engagement. The lane is the executor; this only
    /// reads its receipts, so the evaluator never has to trust its own memory.
    async fn record_replies_as_touches(
        &self,
        ws: Uuid,
        report: &mut SweepReport,
    ) -> Result<(), SweepError> {
        let replies = sqlx::query_as::<_, ReplyRow>(
            "SELECT c.id AS comment_id,
                    c.replied_at,
                    o.prospect_id,
                    link.id AS smart_link_id
             FROM community_comments c
             JOIN fan_prospect_observations o
               ON o.workspace_id = c.workspace_id
              AND o.source = 'own_comments'
              AND o.source_ref = c.id::text
             LEFT JOIN smart_links link
               ON link.workspace_id = c.workspace_id
              AND link.slug = 'reply-capture-' || replace(c.id::text, '-', '')
             WHERE c.workspace_id = $1
               AND c.status = 'replied'
               AND c.replied_at IS NOT NULL
             ORDER BY c.replied_at DESC
             LIMIT $2",
        )
        .bind(ws)
        .bind(OBSERVE_PER_PASS)
        .fetch_all(&self.pool)
        .await?;
        for reply in &replies {
            let source_ref = reply.comment_id.to_string();
            let recorded = record_touch(
                &self.pool,
                ws,
                &TouchReceipt {
                    prospect_id: reply.prospect_id,
                    kind: if reply.smart_link_id.is_some() {
                        TouchKind::Invite
                    } else {
                        TouchKind::Engage
                    },
                    source: "owned_reply",
                    source_ref: &source_ref,
                    smart_link_id: reply.smart_link_id,
                    touched_at: reply.replied_at,
                },
            )
            .await?;
            if recorded {
                report.touched += 1;
            }
        }
        Ok(())
    }
}

#[derive(Debug, FromRow)]
struct ReplyRow {
    comment_id: Uuid,
    replied_at: OffsetDateTime,
    prospect_id: Uuid,
    smart_link_id: Option<Uuid>,
}
