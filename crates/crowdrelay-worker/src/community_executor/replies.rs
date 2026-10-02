//! The reply lane: the band answering the people who comment on its posts.
//!
//! `crowdrelay_domain::community_reply` decides; this module moves the rows.
//! Three steps, each bounded per cycle:
//!
//! 1. **Harvest** — only when a metrics read shows a post's comment count grew
//!    past what was last harvested, one comments read through the agents
//!    service. The comments that are the band's to answer become
//!    `unanswered` rows.
//! 2. **Draft** — the agents service drafts a reply from the post, the band
//!    profile and the band's voice samples, or says why not to reply. The
//!    draft passes the publish guard and the register guard, then an
//!    independent review on a different model; the domain routes it to a
//!    person or, when unattended replies are on and all of that came back
//!    clean, straight to approved.
//! 3. **Send** — one approved reply at a time, never before its randomised
//!    `not_before`, spaced from the last, under a daily ceiling, and not at
//!    all while the account's standing is halted or writes are off.
//!
//! Also home to `record_post_metrics`, which is where a grown comment count
//! is first seen.

use super::*;
use crowdrelay_domain::community_register::review_community_register;
use crowdrelay_domain::community_reply::{
    HarvestedComment, MAX_REPLIES_PER_24H, MIN_REPLY_GAP, ReplyRoute, ReviewOutcome,
    comments_to_answer, reply_not_before, route_reply,
};
use crowdrelay_domain::reddit_standing::{RedditStanding, reddit_standing};

/// Drafts attempted per cycle — each is two free-model calls.
const DRAFTS_PER_CYCLE: i64 = 3;
/// Comments harvested per post read, at most.
const HARVEST_PER_POST: usize = 25;
/// How long a reply may sit in `replying` before it is treated as crashed.
const REPLYING_STALE: Duration = Duration::from_secs(600);
/// Backoff for a draft or send the agents service could not serve.
const RETRY_BACKOFF_MINUTES: i32 = 30;
/// Attempts before a draft or send gives up.
const MAX_ATTEMPTS: i32 = 5;
const AGENTS_DRAFT_TIMEOUT: Duration = Duration::from_secs(90);

/// Whether unattended replies are switched on. Off unless set: every reply
/// waits for a person by default, and the Reddit write switches still apply.
fn unattended_replies_enabled() -> bool {
    std::env::var("CROWDRELAY_REDDIT_AUTO_REPLY")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "true" | "1" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// A uniform draw in `[0, 1)` for reply timing. Falls back to the midpoint
/// when the OS has no randomness to give — timing, not security.
fn unit_draw() -> f64 {
    let mut bytes = [0u8; 4];
    if getrandom::fill(&mut bytes).is_err() {
        return 0.5;
    }
    f64::from(u32::from_le_bytes(bytes)) / (f64::from(u32::MAX) + 1.0)
}

fn post_fullname(reddit_post_id: &str) -> String {
    if reddit_post_id.starts_with("t3_") {
        reddit_post_id.to_owned()
    } else {
        format!("t3_{reddit_post_id}")
    }
}

/// The `/community/review` request body. `drafted_by_provider` is absent — not
/// null — when the draft carries no provider: the agents schema is
/// `z.string().optional()`, and zod's optional accepts a missing key but
/// rejects an explicit `null` ("Expected string, received null"), which the
/// caller reads as an unreachable reviewer and parks the draft for a person.
fn review_payload(
    kind: &str,
    subreddit: &str,
    text: &str,
    context: &str,
    drafted_by_provider: Option<&str>,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "kind": kind,
        "subreddit": subreddit,
        "text": text.chars().take(4000).collect::<String>(),
        "context": context.chars().take(4000).collect::<String>(),
    });
    if let (Some(provider), Some(object)) = (drafted_by_provider, payload.as_object_mut()) {
        object.insert("drafted_by_provider".to_owned(), provider.into());
    }
    payload
}

#[derive(Deserialize)]
struct AgentComment {
    id: String,
    parent_id: String,
    author: String,
    body: String,
    #[serde(default)]
    is_submitter: bool,
    #[serde(default)]
    gone: bool,
}

#[derive(Deserialize)]
struct AgentComments {
    comments: Vec<AgentComment>,
}

#[derive(Deserialize)]
struct ReplyDraft {
    reply: Option<String>,
    skip_reason: Option<String>,
    provider: Option<String>,
    model: Option<String>,
}

fn owned_reply_capture_slug(comment_id: Uuid) -> String {
    format!("reply-capture-{}", comment_id.simple())
}

#[derive(Deserialize)]
struct Review {
    score: f64,
    pass: bool,
}

#[derive(Deserialize)]
struct ReplySent {
    comment_id: String,
    permalink: Option<String>,
}

#[derive(sqlx::FromRow)]
struct DraftRow {
    id: Uuid,
    platform: String,
    parent_id: String,
    author: String,
    body: String,
    subreddit: String,
    post_title: String,
    post_body: String,
    parent_body: Option<String>,
    parent_by_band: bool,
    attempts: i32,
}

#[derive(sqlx::FromRow)]
struct SendRow {
    id: Uuid,
    platform_comment_id: String,
    draft: String,
    attempts: i32,
}

impl CommunityExecutorWorker {
    /// One JSON call to the agents service with a capability-scoped token.
    /// 429 → `RateLimited`, 5xx → `SessionUnavailable` (transient), other
    /// non-success → `RedditApi` (terminal for the item).
    async fn agents_call<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        capability: crate::discovery::AgentCapability,
        payload: &Value,
        timeout: Duration,
    ) -> Result<T, CommunityExecutorError> {
        let auth_key = self
            .agent_service_auth_key
            .as_deref()
            .ok_or(CommunityExecutorError::NoAgentsService)?;
        let ws = self.workspace_id.into_uuid();
        let token = crate::discovery::derive_agent_token_with_capability(auth_key, ws, capability);
        let client = self
            .http_client
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let response = client
            .post(format!("{}{path}", self.agent_service_url))
            .header("Authorization", format!("Bearer {token}"))
            .header("X-Workspace-Id", ws.to_string())
            .json(payload)
            .timeout(timeout)
            .send()
            .await?;
        let status = response.status();
        if status.as_u16() == 429 {
            return Err(CommunityExecutorError::RateLimited);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let message = format!(
                "agents {path} HTTP {status}: {}",
                body.chars().take(300).collect::<String>()
            );
            return Err(if status.is_server_error() {
                CommunityExecutorError::SessionUnavailable(message)
            } else {
                CommunityExecutorError::RedditApi(message)
            });
        }
        check_response_size(&response)?;
        Ok(response.json().await?)
    }

    /// Records a post metrics snapshot and updates the fetch timestamp, then
    /// harvests comments when the count grew.
    pub(super) async fn record_post_metrics(
        &self,
        post_id: Uuid,
        reddit_post_id: &str,
        metrics: &RedditPostMetrics,
    ) -> Result<(), CommunityExecutorError> {
        let ws = self.workspace_id.into_uuid();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
            INSERT INTO community_post_metrics
                (workspace_id, community_post_id, reddit_post_id, score, upvotes, num_comments, upvote_ratio)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (workspace_id, community_post_id, measured_at) DO NOTHING
            "#,
        )
        .bind(ws)
        .bind(post_id)
        .bind(reddit_post_id)
        .bind(metrics.score)
        .bind(metrics.upvotes)
        .bind(metrics.num_comments)
        .bind(metrics.upvote_ratio)
        .execute(&mut *tx)
        .await?;
        standing::record_removal_state(&mut tx, ws, post_id, metrics).await?;
        sqlx::query(
            r#"
            UPDATE community_posts
            SET metrics_last_fetched_at = now(),
                updated_at = now()
            WHERE id = $1 AND workspace_id = $2
            "#,
        )
        .bind(post_id)
        .bind(ws)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        // A harvest failure is logged, never fatal: the metrics are recorded
        // and the next grown count tries again.
        if let Err(error) = self
            .harvest_comments(post_id, reddit_post_id, metrics.num_comments)
            .await
        {
            tracing::warn!(%post_id, error = %error, "comment harvest failed");
        }
        Ok(())
    }

    /// Reads the post's comments when its count grew past the last harvest.
    async fn harvest_comments(
        &self,
        post_id: Uuid,
        reddit_post_id: &str,
        num_comments: i32,
    ) -> Result<(), CommunityExecutorError> {
        let ws = self.workspace_id.into_uuid();
        let seen: i32 = sqlx::query_scalar(
            "SELECT comments_seen FROM community_posts WHERE id = $1 AND workspace_id = $2",
        )
        .bind(post_id)
        .bind(ws)
        .fetch_one(&self.pool)
        .await?;
        if num_comments <= seen || self.agent_service_auth_key.is_none() {
            return Ok(());
        }
        let read: AgentComments = self
            .agents_call(
                "/reddit/comments",
                crate::discovery::AgentCapability::Read,
                &serde_json::json!({ "post_id": reddit_post_id }),
                AGENTS_METRICS_TIMEOUT,
            )
            .await?;
        let harvested: Vec<HarvestedComment> = read
            .comments
            .into_iter()
            .map(|c| HarvestedComment {
                id: c.id,
                parent_id: c.parent_id,
                author: c.author,
                body: c.body,
                by_band: c.is_submitter,
                gone: c.gone,
            })
            .collect();
        let mut tx = self.pool.begin().await?;
        for comment in comments_to_answer(&post_fullname(reddit_post_id), &harvested)
            .into_iter()
            .take(HARVEST_PER_POST)
        {
            // The thread is in hand: keep what this comment answers, and who
            // said it, so the draft continues the real conversation.
            let parent = harvested.iter().find(|c| c.id == comment.parent_id);
            sqlx::query(
                r#"
                INSERT INTO community_comments
                    (workspace_id, community_post_id, platform_comment_id, parent_id, author, body,
                     parent_body, parent_by_band)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                ON CONFLICT (workspace_id, platform, platform_comment_id) DO NOTHING
                "#,
            )
            .bind(ws)
            .bind(post_id)
            .bind(&comment.id)
            .bind(&comment.parent_id)
            .bind(comment.author.chars().take(64).collect::<String>())
            .bind(comment.body.chars().take(4000).collect::<String>())
            .bind(parent.map(|p| p.body.chars().take(4000).collect::<String>()))
            .bind(parent.is_some_and(|p| p.by_band))
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "UPDATE community_posts SET comments_seen = $3, updated_at = now() WHERE id = $1 AND workspace_id = $2",
        )
        .bind(post_id)
        .bind(ws)
        .bind(num_comments.max(0))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Drafts pending comments, then sends at most one due reply.
    ///
    /// Public so tests can drive the real lane — the same reason
    /// `recover_stale_posting` is public: the regressions it guards live in
    /// the SQL this runs, not in a seam a test could replay by hand.
    pub async fn run_reply_lane(&self) -> Result<usize, CommunityExecutorError> {
        if self.agent_service_auth_key.is_none() {
            return Ok(0);
        }
        self.recover_stale_replies().await?;
        let harvested = self.harvest_owned_comments().await.unwrap_or_else(|error| {
            tracing::warn!(error = %error, "owned-channel comment harvest failed");
            0
        });
        // FAN SCOUT is part of the reply control path, not a dashboard lagging
        // an hour behind it. Observe fresh comments and ingest prior send
        // receipts before deciding whether another person should hear from us.
        let scout = crate::prospect_sweep::ProspectSweep::new(
            self.pool.clone(),
            self.workspace_id,
            self.operation_timeout,
        );
        if let Err(error) = scout.run_once(OffsetDateTime::now_utc()).await {
            tracing::warn!(error = %error, "fan scout pre-reply sweep failed");
        }
        let drafted = self.draft_pending_replies().await?;
        let sent = self.send_due_reply().await? + self.send_due_owned_reply().await?;
        Ok(harvested + drafted + sent)
    }

    /// A reply stuck in `replying` may already be live on Reddit. It is not
    /// sent again — it goes to a person, who can see the thread.
    async fn recover_stale_replies(&self) -> Result<(), CommunityExecutorError> {
        sqlx::query(
            r#"
            UPDATE community_comments
            SET status = 'awaiting_approval',
                hold_reason = 'held: the send was interrupted — check the thread before approving again, it may already be posted',
                updated_at = now()
            WHERE workspace_id = $1
              AND status = 'replying'
              AND updated_at < now() - make_interval(secs => $2::double precision)
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(REPLYING_STALE.as_secs() as f64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn draft_pending_replies(&self) -> Result<usize, CommunityExecutorError> {
        let ws = self.workspace_id.into_uuid();
        let rows: Vec<DraftRow> = sqlx::query_as(
            r#"
            -- A Reddit comment's post is a community post; an Instagram or
            -- Facebook comment's post is the synced social post itself.
            SELECT c.id, c.platform, c.parent_id, c.author, c.body,
                   COALESCE(p.subreddit, c.platform) AS subreddit,
                   COALESCE(p.title, src.title, '') AS post_title,
                   COALESCE(p.body, src.metadata->>'body', '') AS post_body,
                   c.parent_body, c.parent_by_band, c.attempts
            FROM community_comments c
            LEFT JOIN community_posts p
              ON p.id = c.community_post_id AND p.workspace_id = c.workspace_id
            LEFT JOIN content_sources src
              ON src.id = c.content_source_id AND src.workspace_id = c.workspace_id
            WHERE c.workspace_id = $1
              AND c.status = 'unanswered'
              AND (c.not_before IS NULL OR c.not_before <= now())
            ORDER BY c.created_at
            LIMIT $2
            "#,
        )
        .bind(ws)
        .bind(DRAFTS_PER_CYCLE)
        .fetch_all(&self.pool)
        .await?;
        let recent: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT draft FROM community_comments
            WHERE workspace_id = $1 AND status = 'replied' AND platform = 'reddit'
              AND replied_at > now() - INTERVAL '30 days'
            "#,
        )
        .bind(ws)
        .fetch_all(&self.pool)
        .await?;
        let mut drafted = 0;
        for row in rows {
            match self.draft_one(&row, &recent).await {
                Ok(()) => drafted += 1,
                Err(error) => {
                    tracing::warn!(comment = %row.id, error = %error, "reply draft failed");
                    self.back_off(row.id, row.attempts + 1, "unanswered")
                        .await?;
                }
            }
        }
        Ok(drafted)
    }

    async fn draft_one(
        &self,
        row: &DraftRow,
        recent: &[String],
    ) -> Result<(), CommunityExecutorError> {
        let ws = self.workspace_id.into_uuid();
        let fan_scout_action = if matches!(row.platform.as_str(), "instagram" | "facebook") {
            crowdrelay_infra::fan_prospects::next_action_for_comment(&self.pool, ws, row.id)
                .await
                .map_err(|error| match error {
                    crowdrelay_infra::fan_prospects::ProspectError::Database(error) => {
                        CommunityExecutorError::Database(error)
                    }
                })?
        } else {
            None
        };

        if matches!(row.platform.as_str(), "instagram" | "facebook") {
            use crowdrelay_domain::fan_next_action::FanProspectActionKind as FanAction;
            match fan_scout_action.as_ref().map(|action| action.decision.action) {
                Some(FanAction::EngageInContext | FanAction::InviteToFanbase) => {}
                Some(FanAction::DoNotContact) => {
                    sqlx::query(
                        "UPDATE community_comments
                         SET status='skipped',
                             hold_reason='FAN SCOUT: person is refused/suppressed; do not contact',
                             updated_at=now()
                         WHERE id=$1 AND workspace_id=$2 AND status='unanswered'",
                    )
                    .bind(row.id)
                    .bind(ws)
                    .execute(&self.pool)
                    .await?;
                    return Ok(());
                }
                Some(FanAction::Observe) => {
                    sqlx::query(
                        "UPDATE community_comments
                         SET status='skipped',
                             hold_reason='FAN SCOUT: observe only; no active relationship move is justified',
                             updated_at=now()
                         WHERE id=$1 AND workspace_id=$2 AND status='unanswered'",
                    )
                    .bind(row.id)
                    .bind(ws)
                    .execute(&self.pool)
                    .await?;
                    return Ok(());
                }
                Some(FanAction::Hold) => {
                    // Holds are re-evaluated without model spend. Six hours is
                    // only the polling cadence; the typed evaluator still owns
                    // the actual 72h/168h relationship cooldown.
                    sqlx::query(
                        "UPDATE community_comments
                         SET not_before=now()+INTERVAL '6 hours',
                             hold_reason=$3,
                             updated_at=now()
                         WHERE id=$1 AND workspace_id=$2 AND status='unanswered'",
                    )
                    .bind(row.id)
                    .bind(ws)
                    .bind(
                        fan_scout_action.as_ref().map_or_else(
                            || "FAN SCOUT: held".to_owned(),
                            |action| format!("FAN SCOUT: {}", action.decision.reason),
                        ),
                    )
                    .execute(&self.pool)
                    .await?;
                    return Ok(());
                }
                None => {
                    // The discovery sweep may have failed or the public author
                    // may not be a safe identity. Never fall back from "Brain
                    // has no person decision" to "ask an LLM what to do".
                    sqlx::query(
                        "UPDATE community_comments
                         SET not_before=now()+INTERVAL '15 minutes',
                             hold_reason='FAN SCOUT: prospect evidence not ready; no model action taken',
                             updated_at=now()
                         WHERE id=$1 AND workspace_id=$2 AND status='unanswered'",
                    )
                    .bind(row.id)
                    .bind(ws)
                    .execute(&self.pool)
                    .await?;
                    return Ok(());
                }
            }
        }

        // What this comment answers, stored at harvest: the band's own words
        // when a fan is answering the band. Rows harvested before the parent
        // was kept fall back to naming the band without quoting it.
        let thread = match (&row.parent_body, row.parent_id.starts_with("t1_")) {
            (Some(body), _) => vec![serde_json::json!({
                "author": if row.parent_by_band { "the band" } else { "someone" },
                "body": body,
                "is_band": row.parent_by_band,
            })],
            (None, true) => vec![serde_json::json!({
                "author": "the band",
                "body": "(the band's earlier reply in this thread)",
                "is_band": true,
            })],
            (None, false) => Vec::new(),
        };
        let draft: ReplyDraft = self
            .agents_call(
                "/community/reply-draft",
                crate::discovery::AgentCapability::Dispatch,
                &serde_json::json!({
                    "platform": row.platform,
                    "subreddit": row.subreddit,
                    "post_title": row.post_title,
                    "post_body": row.post_body,
                    "thread": thread,
                    "comment": { "author": row.author, "body": row.body },
                }),
                AGENTS_DRAFT_TIMEOUT,
            )
            .await?;
        let Some(reply) = draft
            .reply
            .as_deref()
            .map(|r| r.trim().to_owned())
            .filter(|r| !r.is_empty())
        else {
            let reason = draft
                .skip_reason
                .unwrap_or_else(|| "the drafter chose not to reply".to_owned());
            if fan_scout_action.is_some() {
                // Brain selected a relationship move. A wording model may fail
                // to serve it, but it cannot silently turn that decision into
                // permanent silence. Retry a bounded number of times, then
                // surface the failed wording task for an operator.
                let attempts = row.attempts.saturating_add(1).max(1);
                let failed = attempts >= MAX_ATTEMPTS;
                let hold = if failed {
                    format!(
                        "FAN SCOUT: gave up after {attempts} wording attempts for the selected action — last drafter reason: {reason}"
                    )
                } else {
                    format!(
                        "FAN SCOUT: selected action still needs wording — drafter returned no reply: {reason}"
                    )
                };
                sqlx::query(
                    r#"
                    UPDATE community_comments
                    SET status = CASE WHEN $3 THEN 'failed' ELSE 'unanswered' END,
                        hold_reason = $4,
                        drafted_by = $5,
                        attempts = GREATEST(attempts, $6),
                        not_before = now() + make_interval(mins => $7),
                        updated_at = now()
                    WHERE id=$1 AND workspace_id=$2 AND status='unanswered'
                    "#,
                )
                .bind(row.id)
                .bind(ws)
                .bind(failed)
                .bind(hold.chars().take(500).collect::<String>())
                .bind(draft.model.as_deref())
                .bind(attempts)
                .bind(RETRY_BACKOFF_MINUTES)
                .execute(&self.pool)
                .await?;
            } else {
                // Legacy/non-prospect reply lanes retain their current
                // semantics: the model may decide a conversation needs no
                // answer at all.
                sqlx::query(
                    "UPDATE community_comments SET status = 'skipped', hold_reason = $3, drafted_by = $4, updated_at = now() WHERE id = $1 AND workspace_id = $2",
                )
                .bind(row.id)
                .bind(ws)
                .bind(reason.chars().take(500).collect::<String>())
                .bind(draft.model.as_deref())
                .execute(&self.pool)
                .await?;
            }
            return Ok(());
        };

        // The mechanical checks a post passes. `Social` bounds a reply's
        // length; no approved origins means any link holds — the post already
        // carries one.
        let hashes: BTreeSet<String> = recent.iter().map(|r| content_hash(r)).collect();
        let guard_hold = review_outbound_post(
            &reply,
            &PublishContext {
                channel: PublishChannel::Social,
                approved_origins: &[],
                approved_links: &[],
                recent_content_hashes: &hashes,
                dedupe_text: None,
            },
        )
        .hold_reason()
        .map(|reason| reason.as_str().to_owned())
        // The register is the community channel's own — the band's Instagram
        // or Facebook audience opted into its hashtags and calls to action.
        .or_else(|| {
            (row.platform == "reddit")
                .then(|| review_community_register(&reply, recent))
                .flatten()
                .map(|hold| hold.as_str().to_owned())
        });

        let review = if guard_hold.is_some() {
            // Already going to a person; no need to spend a review.
            ReviewOutcome::Unavailable
        } else {
            self.review_text(
                if row.platform == "reddit" {
                    "reddit_reply"
                } else {
                    "owned_reply"
                },
                &row.subreddit,
                &reply,
                &format!(
                    "Post title: {}\nPost body: {}\nComment by {}: {}",
                    row.post_title, row.post_body, row.author, row.body
                ),
                draft.provider.as_deref(),
            )
            .await
        };
        let review_score = match review {
            ReviewOutcome::Passed { score } | ReviewOutcome::Failed { score } => {
                Some(i16::from(score))
            }
            ReviewOutcome::Unavailable => None,
        };

        // CTA authority comes from the typed FAN SCOUT decision, never from
        // the copy model. The model writes natural wording; a deterministic
        // InviteToFanbase decision is the only thing that may attach this
        // tracked first-party path.
        let fan_scout_invite = fan_scout_action.as_ref().is_some_and(|action| {
            action.decision.action
                == crowdrelay_domain::fan_next_action::FanProspectActionKind::InviteToFanbase
                && action.decision.cta_intent
                    == Some(crowdrelay_domain::fan_next_action::FanProspectCtaIntent::JoinFanbase)
        });
        let capture_url_chars = self.public_origin.trim_end_matches('/').chars().count()
            + "/l/".chars().count()
            + owned_reply_capture_slug(row.id).chars().count();
        let capture_fits = reply.chars().count() + 2 + capture_url_chars <= 400;
        let capture_link = if guard_hold.is_none()
            && matches!(review, ReviewOutcome::Passed { .. })
            && fan_scout_invite
            && capture_fits
        {
            self.owned_reply_capture_link(row).await?
        } else {
            None
        };
        let reply = capture_link
            .as_ref()
            .map_or(reply.clone(), |link| format!("{reply}\n\n{link}"));
        let capture_added = capture_link
            .as_ref()
            .is_some_and(|link| reply.contains(link.as_str()));

        // Each channel's own pair of switches: Reddit's write switches, or
        // the owned-channel publish gate.
        let unattended = if row.platform == "reddit" {
            unattended_replies_enabled() && reddit_write_enabled()
        } else {
            owned_replies::unattended_owned_replies_enabled()
        };
        let (status, hold_reason, approved_by, not_before) = if capture_added {
            (
                "awaiting_approval",
                Some(
                    "held: FAN SCOUT selected InviteToFanbase from explicit join/follow evidence — tracked capture link added; review before sending"
                        .to_owned(),
                ),
                None,
                None,
            )
        } else if fan_scout_invite {
            // An invite decision without its reviewed tracked path must never
            // silently degrade into a plain auto-reply. A person can repair or
            // decline it; the machine cannot substitute a different action.
            (
                "awaiting_approval",
                Some(
                    "held: FAN SCOUT selected InviteToFanbase, but the tracked CTA could not be safely attached"
                        .to_owned(),
                ),
                None,
                None,
            )
        } else {
            match route_reply(guard_hold.as_deref(), review, unattended) {
                ReplyRoute::Approve => (
                    "approved",
                    None,
                    Some("unattended: clean draft, review passed"),
                    Some(reply_not_before(OffsetDateTime::now_utc(), unit_draw())),
                ),
                ReplyRoute::AwaitApproval { reason } => ("awaiting_approval", reason, None, None),
            }
        };
        sqlx::query(
            r#"
            UPDATE community_comments
            SET status = $3, draft = $4, hold_reason = $5, review_score = $6,
                drafted_by = $7, approved_by = $8, not_before = $9, updated_at = now()
            WHERE id = $1 AND workspace_id = $2 AND status = 'unanswered'
            "#,
        )
        .bind(row.id)
        .bind(ws)
        .bind(status)
        .bind(&reply)
        .bind(hold_reason)
        .bind(review_score)
        .bind(draft.model.as_deref())
        .bind(approved_by)
        .bind(not_before)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mints the tenant-owned tracked link only after FAN SCOUT selected
    /// InviteToFanbase. The smart-link dimensions make
    /// click → visitor → fan attribution readable without adding a second
    /// reply ledger.
    async fn owned_reply_capture_link(
        &self,
        row: &DraftRow,
    ) -> Result<Option<String>, CommunityExecutorError> {
        let snapshot = crowdrelay_infra::join_ask::load_join_ask_snapshot(
            &self.pool,
            self.workspace_id.into_uuid(),
        )
        .await?;
        let Some(base) = snapshot.member_site_base_url else {
            return Ok(None);
        };
        let slug = owned_reply_capture_slug(row.id);
        let destination = format!(
            "{}/signal?utm_source={}&utm_medium=comment_reply&utm_campaign=owned_reply_capture&utm_content={}",
            base.trim_end_matches('/'),
            row.platform,
            row.id.simple()
        );
        sqlx::query(
            r#"
            INSERT INTO smart_links
                (workspace_id, slug, destination_url, active,
                 channel_source, channel_community, channel_creative)
            VALUES ($1,$2,$3,true,$4,$5,'owned_reply_capture')
            ON CONFLICT (workspace_id, slug) DO UPDATE SET
                destination_url = EXCLUDED.destination_url,
                active = true,
                channel_source = EXCLUDED.channel_source,
                channel_community = EXCLUDED.channel_community,
                channel_creative = EXCLUDED.channel_creative
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(&slug)
        .bind(destination)
        .bind(&row.platform)
        .bind(format!("comment:{}", row.id))
        .execute(&self.pool)
        .await?;
        Ok(Some(format!(
            "{}/l/{slug}",
            self.public_origin.trim_end_matches('/')
        )))
    }

    /// The independent review of a community post or reply. Any failure to
    /// get one is `Unavailable` — never a pass.
    pub(super) async fn review_text(
        &self,
        kind: &str,
        subreddit: &str,
        text: &str,
        context: &str,
        drafted_by_provider: Option<&str>,
    ) -> ReviewOutcome {
        let payload = review_payload(kind, subreddit, text, context, drafted_by_provider);
        match self
            .agents_call::<Review>(
                "/community/review",
                crate::discovery::AgentCapability::Dispatch,
                &payload,
                AGENTS_DRAFT_TIMEOUT,
            )
            .await
        {
            Ok(review) => {
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "clamped to 0..=10 first"
                )]
                let score = review.score.clamp(0.0, 10.0).round() as u8;
                if review.pass {
                    ReviewOutcome::Passed { score }
                } else {
                    ReviewOutcome::Failed { score }
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "independent review unavailable");
                ReviewOutcome::Unavailable
            }
        }
    }

    async fn send_due_reply(&self) -> Result<usize, CommunityExecutorError> {
        if self.manual_mode || !reddit_write_enabled() {
            return Ok(0);
        }
        let ws = self.workspace_id.into_uuid();
        let history = standing::post_history(&self.pool, ws).await?;
        if let RedditStanding::Halted(reason) = reddit_standing(&history, OffsetDateTime::now_utc())
        {
            // Everything approved goes back to a person while the account is
            // halted — a reply is a write through the same account.
            sqlx::query(
                "UPDATE community_comments SET status = 'awaiting_approval', hold_reason = $2, updated_at = now() WHERE workspace_id = $1 AND status = 'approved' AND platform = 'reddit'",
            )
            .bind(ws)
            .bind(reason.as_str())
            .execute(&self.pool)
            .await?;
            return Ok(0);
        }
        let (sent_24h, last_sent): (i64, Option<OffsetDateTime>) = sqlx::query_as(
            r#"
            SELECT count(*) FILTER (WHERE replied_at > now() - INTERVAL '24 hours'),
                   max(replied_at)
            FROM community_comments
            WHERE workspace_id = $1 AND status = 'replied' AND platform = 'reddit'
            "#,
        )
        .bind(ws)
        .fetch_one(&self.pool)
        .await?;
        if sent_24h >= MAX_REPLIES_PER_24H
            || last_sent.is_some_and(|at| OffsetDateTime::now_utc() - at < MIN_REPLY_GAP)
        {
            return Ok(0);
        }

        let mut tx = self.pool.begin().await?;
        let row: Option<SendRow> = sqlx::query_as(
            r#"
            SELECT id, platform_comment_id, draft, attempts
            FROM community_comments
            WHERE workspace_id = $1 AND status = 'approved' AND platform = 'reddit'
              AND (not_before IS NULL OR not_before <= now())
            ORDER BY not_before NULLS FIRST, created_at
            LIMIT 1
            FOR UPDATE SKIP LOCKED
            "#,
        )
        .bind(ws)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Ok(0);
        };
        sqlx::query(
            "UPDATE community_comments SET status = 'replying', attempts = attempts + 1, updated_at = now() WHERE id = $1 AND workspace_id = $2",
        )
        .bind(row.id)
        .bind(ws)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        match self
            .agents_call::<ReplySent>(
                "/reddit/reply",
                crate::discovery::AgentCapability::SocialPublish,
                &serde_json::json!({ "parent_id": row.platform_comment_id, "text": row.draft }),
                AGENTS_SUBMIT_TIMEOUT,
            )
            .await
        {
            Ok(sent) => {
                sqlx::query(
                    r#"
                    UPDATE community_comments
                    SET status = 'replied', reply_comment_id = $3, reply_permalink = $4,
                        replied_at = now(), hold_reason = NULL, updated_at = now()
                    WHERE id = $1 AND workspace_id = $2
                    "#,
                )
                .bind(row.id)
                .bind(ws)
                .bind(&sent.comment_id)
                .bind(
                    sent.permalink
                        .map(|p| p.chars().take(500).collect::<String>()),
                )
                .execute(&self.pool)
                .await?;
                Ok(1)
            }
            Err(CommunityExecutorError::RedditApi(message)) => {
                // Reddit (or the route) refused this reply — terminal for it.
                sqlx::query(
                    "UPDATE community_comments SET status = 'failed', hold_reason = $3, updated_at = now() WHERE id = $1 AND workspace_id = $2",
                )
                .bind(row.id)
                .bind(ws)
                .bind(message.chars().take(500).collect::<String>())
                .execute(&self.pool)
                .await?;
                Ok(0)
            }
            Err(error) => {
                tracing::warn!(comment = %row.id, error = %error, "reply send deferred");
                self.back_off(row.id, row.attempts + 1, "approved").await?;
                Ok(0)
            }
        }
    }

    /// Defers a row by the retry backoff in `status`, or fails it once it
    /// has used its attempts.
    pub(super) async fn back_off(
        &self,
        id: Uuid,
        attempts: i32,
        status: &str,
    ) -> Result<(), CommunityExecutorError> {
        sqlx::query(
            r#"
            UPDATE community_comments
            SET status = CASE WHEN $3 >= $4 THEN 'failed' ELSE $5 END,
                hold_reason = CASE WHEN $3 >= $4 THEN 'gave up: the agents service could not serve it' ELSE hold_reason END,
                attempts = GREATEST(attempts, $3),
                not_before = now() + make_interval(mins => $6),
                updated_at = now()
            WHERE id = $1 AND workspace_id = $2
            "#,
        )
        .bind(id)
        .bind(self.workspace_id.into_uuid())
        .bind(attempts.max(1))
        .bind(MAX_ATTEMPTS)
        .bind(status)
        .bind(RETRY_BACKOFF_MINUTES)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod replies_tests {
    use super::*;

    #[test]
    fn post_ids_become_fullnames_once() {
        assert_eq!(post_fullname("abc123"), "t3_abc123");
        assert_eq!(post_fullname("t3_abc123"), "t3_abc123");
    }

    #[test]
    fn unattended_replies_are_off_unless_switched_on() {
        assert!(!unattended_replies_enabled());
    }

    #[test]
    fn timing_draws_stay_in_the_unit_interval() {
        for _ in 0..100 {
            let draw = unit_draw();
            assert!((0.0..1.0).contains(&draw), "{draw}");
        }
    }

    /// The agents schema is `z.string().optional()` — zod accepts the key
    /// missing but answers 400 ("Expected string, received null") when it is
    /// explicitly null. A post draft carries no provider, so serializing the
    /// option verbatim held every community post for a person forever.
    #[test]
    fn review_payload_omits_provider_key_when_unknown() {
        let payload = review_payload("reddit_post", "r/test", "body", "ctx", None);
        assert!(
            !payload
                .as_object()
                .unwrap()
                .contains_key("drafted_by_provider")
        );
        assert_eq!(payload["kind"], "reddit_post");
        assert_eq!(payload["subreddit"], "r/test");
    }

    #[test]
    fn review_payload_sends_provider_string_when_known() {
        let payload = review_payload("reddit_reply", "r/test", "body", "ctx", Some("claude"));
        assert_eq!(payload["drafted_by_provider"], "claude");
    }

    #[test]
    fn reply_capture_slug_is_stable_and_comment_scoped() {
        let id = Uuid::parse_str("018f5a00-1234-7abc-8def-0123456789ab").unwrap();
        assert_eq!(
            owned_reply_capture_slug(id),
            "reply-capture-018f5a0012347abc8def0123456789ab"
        );
    }
}
